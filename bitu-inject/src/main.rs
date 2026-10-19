//! bitu-inject: injeta a libbitu_layer.so num processo VIVO via ptrace + dlopen.
//!
//! Fluxo (x86_64, glibc):
//!  1. valida alvo (uid, maps), acha `__libc_dlopen_mode` na libc do alvo;
//!  2. PTRACE_ATTACH no thread principal;
//!  3. hijack de registradores: chama dlopen(nosso .so), espera o retorno;
//!  4. lê os maps de novo, acha a base do nosso .so, resolve
//!     `bitu_inject_install` pela dynsym e chama (instala os hooks int3);
//!  5. restaura registradores e PTRACE_DETACH.
//!
//! A chamada remota termina "retornando para 0" (SIGSEGV). Como estamos com
//! ptrace, o sinal é interceptado por nós e DESCARTADO — o handler de SIGSEGV
//! do jogo (Wine/Proton têm) nunca vê nada.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_int, c_void};
use std::ptr;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("uso: bitu-inject <pid> <libbitu_layer.so>");
        eprintln!("     bitu-inject --spawn <libbitu_layer.so> <programa> [args…]");
        return 2;
    }
    // modo de teste/uso direto: o injetor vira pai do alvo (Yama scope=1 permite)
    if args[1] == "--spawn" {
        if args.len() < 4 {
            eprintln!("uso: bitu-inject --spawn <lib> <programa> [args…]");
            return 2;
        }
        let mut c = std::process::Command::new(&args[3]);
        for a in &args[4..] {
            c.arg(a);
        }
        let mut child = match c.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("erro ao lançar: {e}");
                return 1;
            }
        };
        let pid = child.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(900));
        match injetar(pid, &args[2]) {
            Ok(mask) => println!("pid {pid} ok {mask}"),
            Err(e) => {
                eprintln!("erro: {e}");
                let _ = child.kill();
                let _ = child.wait();
                return 1;
            }
        }
        return match child.wait() {
            Ok(s) => {
                use std::os::unix::process::ExitStatusExt;
                match s.signal() {
                    Some(sig) => {
                        eprintln!("[inj] alvo morreu pelo sinal {sig}");
                        128 + sig
                    }
                    None => s.code().unwrap_or(1),
                }
            }
            Err(_) => 1,
        };
    }
    let pid: i32 = match args[1].parse() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("erro: pid inválido");
            return 2;
        }
    };
    match injetar(pid, &args[2]) {
        Ok(mask) => {
            println!("ok {mask}");
            0
        }
        Err(e) => {
            eprintln!("erro: {e}");
            1
        }
    }
}

fn injetar(pid: i32, lib: &str) -> Result<u32, String> {
    let lib = std::fs::canonicalize(lib).map_err(|e| format!("lib não encontrada: {e}"))?;
    if !lib.is_file() {
        return Err("lib não é um arquivo".to_string());
    }
    let lib_s = lib.to_string_lossy().to_string();
    verificar_alvo(pid)?;

    let maps0 = ler_maps(pid)?;
    let (libc_path, libc_base) = achar_libc(&maps0)?;
    let syms = dynsym(pid, &libc_path, &["__libc_dlopen_mode", "dlopen"])?;
    let dl_off = syms
        .get("__libc_dlopen_mode")
        .or_else(|| syms.get("dlopen"))
        .copied()
        .ok_or("dlopen não encontrado na libc")?;
    let dlopen_addr = libc_base + dl_off;
    if debug_inj() {
        let qual = if syms.contains_key("__libc_dlopen_mode") {
            "__libc_dlopen_mode"
        } else {
            "dlopen"
        };
        eprintln!(
            "[inj] {qual} off={dl_off:#x} base={libc_base:#x} -> {dlopen_addr:#x} ({libc_path:?})"
        );
        eprintln!("[inj] símbolos: {syms:?}");
    }

    attach(pid)?;
    let res = injetar_com_attach(pid, &lib_s, dlopen_addr, &maps0);
    let _ = detach(pid);
    res
}

fn injetar_com_attach(pid: i32, lib: &str, dlopen_addr: u64, maps0: &[Mapa]) -> Result<u32, String> {
    let path_c = CString::new(lib).map_err(|_| "caminho inválido")?;

    // scratch na pilha do alvo: string em rsp-0x2000, chamada em rsp-0x1000
    let regs = pegar_regs(pid)?;
    let rsp = regs.rsp;
    if !mapa_contem(maps0, rsp) {
        return Err("rsp do alvo fora de mapa válido (alvo estranho?)".to_string());
    }
    let str_addr = (rsp - 0x2000) & !0xF;
    escrever(pid, str_addr, path_c.as_bytes_with_nul())?;

    // RTLD_NOW = 2
    let handle = chamar(pid, dlopen_addr, &[str_addr, 2])?;
    if handle == 0 {
        return Err("dlopen retornou NULL (lib recusada pelo alvo)".to_string());
    }

    let maps1 = ler_maps(pid)?;
    let base = maps1
        .iter()
        .filter(|m| m.path == lib)
        .map(|m| m.start)
        .min()
        .ok_or("nosso .so não apareceu nos maps do alvo")?;
    let sym = dynsym(pid, std::path::Path::new(lib), &["bitu_inject_install"])?
        .get("bitu_inject_install")
        .copied()
        .ok_or("símbolo bitu_inject_install ausente no .so")?;
    let install_addr = base + sym;

    let mask = chamar(pid, install_addr, &[])?;
    Ok(mask as u32)
}

// ---------------------------------------------------------------- validação

fn verificar_alvo(pid: i32) -> Result<(), String> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .map_err(|_| format!("processo {pid} não existe"))?;
    let uid = unsafe { libc::getuid() };
    let uid_alvo = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u32>().ok());
    match uid_alvo {
        Some(u) if u == uid => {}
        Some(_) => return Err("processo de outro usuário".to_string()),
        None => return Err("não consegui ler o uid do processo".to_string()),
    }
    Ok(())
}

// ------------------------------------------------------------------- maps

#[derive(Clone)]
struct Mapa {
    start: u64,
    end: u64,
    offset: u64,
    perms: String,
    path: String,
}

fn ler_maps(pid: i32) -> Result<Vec<Mapa>, String> {
    let txt = std::fs::read_to_string(format!("/proc/{pid}/maps"))
        .map_err(|e| format!("maps: {e}"))?;
    let mut out = Vec::new();
    for l in txt.lines() {
        let mut it = l.split_whitespace();
        let Some(range) = it.next() else { continue };
        let Some(perms) = it.next() else { continue };
        let Some(offset) = it.next() else { continue };
        let _dev = it.next();
        let _ino = it.next();
        let path = it.next().unwrap_or("").to_string();
        let mut rr = range.split('-');
        let (Some(a), Some(b)) = (rr.next(), rr.next()) else { continue };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16)) else {
            continue;
        };
        let offset = u64::from_str_radix(offset, 16).unwrap_or(0);
        out.push(Mapa { start, end, offset, perms: perms.to_string(), path });
    }
    Ok(out)
}

fn mapa_contem(maps: &[Mapa], addr: u64) -> bool {
    maps.iter().any(|m| m.start <= addr && addr < m.end && m.perms.contains('w'))
}

fn achar_libc(maps: &[Mapa]) -> Result<(std::path::PathBuf, u64), String> {
    let mut melhor: Option<(&Mapa, &Mapa)> = None; // (entrada offset 0, alguma entrada)
    for m in maps {
        let Some(nome) = std::path::Path::new(&m.path).file_name() else { continue };
        let nome = nome.to_string_lossy();
        if nome.starts_with("libc.so.6") || nome == "libc.musl-x86_64.so.1" {
            if m.offset == 0 {
                return Ok((std::path::PathBuf::from(&m.path), m.start));
            }
            if melhor.is_none() {
                melhor = Some((m, m));
            }
        }
    }
    if let Some((m, _)) = melhor {
        // sem entrada offset 0 mapeada: base = mínimo start com offset subtraído
        let base = m.start.saturating_sub(m.offset);
        return Ok((std::path::PathBuf::from(&m.path), base));
    }
    Err("libc não encontrada no alvo".to_string())
}

// --------------------------------------------------------------------- ELF

/// Lê um ELF do alvo. Caminhos vindos de /proc/<pid>/maps podem existir só no
/// namespace de mount do alvo (Flatpak/pressure-vessel montam o runtime em
/// /run/parent): nesse caso lê pelo /proc/<pid>/root.
fn ler_elf(pid: i32, path: &std::path::Path) -> Result<Vec<u8>, String> {
    match std::fs::read(path) {
        Ok(d) => Ok(d),
        Err(e) => {
            let rel = path.strip_prefix("/").map(std::path::Path::new).unwrap_or(path);
            let alt = std::path::Path::new(&format!("/proc/{pid}/root")).join(rel);
            std::fs::read(&alt).map_err(|e2| {
                format!(
                    "elf {}: {e}; via {}: {e2}",
                    path.display(),
                    alt.display()
                )
            })
        }
    }
}

/// Offsets (st_value) dos símbolos pedidos na dynsym do ELF64 little-endian.
fn dynsym(pid: i32, path: &std::path::Path, procurados: &[&str]) -> Result<HashMap<String, u64>, String> {
    let data = ler_elf(pid, path)?;
    if data.len() < 64 || &data[..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return Err("ELF64 LE esperado".to_string());
    }
    let u16at = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]) as usize;
    let u32at = |o: usize| u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
    let u64at = |o: usize| {
        u64::from_le_bytes(data[o..o + 8].try_into().unwrap())
    };
    let shoff = u64at(0x28) as usize;
    let shentsize = u16at(0x3a);
    let shnum = u16at(0x3c);
    let mut out = HashMap::new();
    for i in 0..shnum {
        let sh = shoff + i * shentsize;
        if sh + shentsize > data.len() || u32at(sh + 4) != 11 {
            continue; // SHT_DYNSYM
        }
        let sh_offset = u64at(sh + 0x18) as usize;
        let sh_size = u64at(sh + 0x20) as usize;
        let sh_link = u32at(sh + 0x28) as usize;
        let str_hdr = shoff + sh_link * shentsize;
        if str_hdr + shentsize > data.len() {
            continue;
        }
        let stroff = u64at(str_hdr + 0x18) as usize;
        let n = sh_size / 24;
        for s in 0..n {
            let so = sh_offset + s * 24;
            if so + 24 > data.len() {
                break;
            }
            let st_name = u32at(so) as usize;
            let st_shndx = u16at(so + 6);
            let st_value = u64at(so + 8);
            if st_name == 0 || st_shndx == 0 || st_value == 0 {
                continue;
            }
            let nome = cstr(&data, stroff + st_name);
            if procurados.contains(&nome) {
                out.entry(nome.to_string()).or_insert(st_value);
            }
        }
    }
    Ok(out)
}

fn cstr(data: &[u8], off: usize) -> &str {
    if off >= data.len() {
        return "";
    }
    let fim = data[off..].iter().position(|&b| b == 0).map(|p| off + p).unwrap_or(data.len());
    std::str::from_utf8(&data[off..fim]).unwrap_or("")
}

// ------------------------------------------------------------------- ptrace

fn debug_inj() -> bool {
    std::env::var("BITU_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Linha de /proc/<pid>/maps que contém `addr` (diagnóstico).
fn mapa_do(pid: i32, addr: u64) -> Option<String> {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).ok()?;
    for l in maps.lines() {
        let range = l.split_whitespace().next()?;
        let mut it = range.split('-');
        let a = u64::from_str_radix(it.next()?, 16).ok()?;
        let b = u64::from_str_radix(it.next()?, 16).ok()?;
        if a <= addr && addr < b {
            return Some(l.to_string());
        }
    }
    None
}

fn attach(pid: i32) -> Result<(), String> {
    let rc = unsafe { libc::ptrace(libc::PTRACE_ATTACH, pid, ptr::null_mut::<c_void>(), ptr::null_mut::<c_void>()) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EPERM) {
            return Err(
                "permissão negada (ptrace bloqueado). Libere com: \
                 sudo sysctl kernel.yama.ptrace_scope=0"
                    .to_string(),
            );
        }
        return Err(format!("PTRACE_ATTACH: {e}"));
    }
    let mut status: c_int = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    if !libc::WIFSTOPPED(status) {
        return Err("alvo não parou após attach".to_string());
    }
    Ok(())
}

fn detach(pid: i32) -> Result<(), String> {
    let rc = unsafe { libc::ptrace(libc::PTRACE_DETACH, pid, ptr::null_mut::<c_void>(), ptr::null_mut::<c_void>()) };
    if rc < 0 {
        return Err(format!("PTRACE_DETACH: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

fn pegar_regs(pid: i32) -> Result<libc::user_regs_struct, String> {
    let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::ptrace(
            libc::PTRACE_GETREGS,
            pid,
            ptr::null_mut::<c_void>(),
            &mut regs as *mut _ as *mut c_void,
        )
    };
    if rc < 0 {
        return Err(format!("GETREGS: {}", std::io::Error::last_os_error()));
    }
    Ok(regs)
}

fn setar_regs(pid: i32, regs: &libc::user_regs_struct) -> Result<(), String> {
    let rc = unsafe {
        libc::ptrace(
            libc::PTRACE_SETREGS,
            pid,
            ptr::null_mut::<c_void>(),
            regs as *const _ as *mut c_void,
        )
    };
    if rc < 0 {
        return Err(format!("SETREGS: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

fn escrever(pid: i32, mut addr: u64, bytes: &[u8]) -> Result<(), String> {
    let mut i = 0;
    while i < bytes.len() {
        let mut word = [0u8; 8];
        let n = (bytes.len() - i).min(8);
        word[..n].copy_from_slice(&bytes[i..i + n]);
        let w = u64::from_le_bytes(word);
        let rc = unsafe {
            libc::ptrace(
                libc::PTRACE_POKEDATA,
                pid,
                addr as *mut c_void,
                w as *mut c_void,
            )
        };
        if rc < 0 {
            return Err(format!("POKEDATA: {}", std::io::Error::last_os_error()));
        }
        addr += 8;
        i += 8;
    }
    Ok(())
}

/// Chama uma função no alvo: retorna rax. Salva/restaura os registradores.
fn chamar(pid: i32, fn_addr: u64, args: &[u64]) -> Result<u64, String> {
    let orig = pegar_regs(pid)?;
    let mut r = orig;

    // topo de chamada: alinhado e com retorno para 0 (SIGSEGV interceptado por nós)
    let stack = ((orig.rsp - 0x1000) & !0xF) - 8;
    escrever(pid, stack, &[0u8; 8])?;

    r.rip = fn_addr;
    r.rdi = *args.first().unwrap_or(&0);
    r.rsi = *args.get(1).unwrap_or(&0);
    r.rdx = *args.get(2).unwrap_or(&0);
    r.rcx = *args.get(3).unwrap_or(&0);
    r.rsp = stack;
    // Se o alvo estava bloqueado num syscall no attach, o kernel pode aplicar
    // "syscall restart" e reescrever RIP em cima do nosso — orig_rax = -1 cancela.
    r.orig_rax = u64::MAX;
    setar_regs(pid, &r)?;
    if debug_inj() {
        let rd = pegar_regs(pid)?;
        eprintln!(
            "[inj] regs pós-set: rip={:#x} rsp={:#x} (alvo {fn_addr:#x}/{stack:#x}) orig.rip={:#x} orig.rsp={:#x}",
            rd.rip, rd.rsp, orig.rip, orig.rsp
        );
    }

    let rc = unsafe { libc::ptrace(libc::PTRACE_CONT, pid, ptr::null_mut::<c_void>(), ptr::null_mut::<c_void>()) };
    if rc < 0 {
        return Err(format!("CONT: {}", std::io::Error::last_os_error()));
    }

    loop {
        let mut status: c_int = 0;
        let w = unsafe { libc::waitpid(pid, &mut status, 0) };
        if w < 0 {
            let _ = setar_regs(pid, &orig);
            return Err(format!("waitpid: {}", std::io::Error::last_os_error()));
        }
        if debug_inj() {
            eprintln!(
                "[inj] wait status={status:#x} stopped={} sig={} exited={} signaled={}",
                libc::WIFSTOPPED(status),
                if libc::WIFSTOPPED(status) { libc::WSTOPSIG(status) } else { 0 },
                libc::WIFEXITED(status),
                libc::WIFSIGNALED(status)
            );
        }
        if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
            let extra = if libc::WIFSIGNALED(status) {
                let mut det = String::new();
                if let Ok(r) = pegar_regs(pid) {
                    det = format!(" rip={:#x} rax={:#x} rsp={:#x}", r.rip, r.rax, r.rsp);
                }
                format!(" (sinal {}{det})", libc::WTERMSIG(status))
            } else {
                format!(" (exit {})", libc::WEXITSTATUS(status))
            };
            return Err(format!("alvo morreu durante a injeção{extra}"));
        }
        if !libc::WIFSTOPPED(status) {
            continue;
        }
        let sig = libc::WSTOPSIG(status);
        let regs = pegar_regs(pid)?;
        if debug_inj() {
            let mut si: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let _ = unsafe {
                libc::ptrace(
                    libc::PTRACE_GETSIGINFO,
                    pid,
                    ptr::null_mut::<c_void>(),
                    &mut si as *mut _ as *mut c_void,
                )
            };
            let mapa = mapa_do(pid, regs.rip);
            eprintln!(
                "[inj] stop sig={sig} code={} si_addr={:#x} rip={:#x} rdi={:#x} rsi={:#x} rdx={:#x} rsp={:#x} rbp={:#x} rax={:#x} rbx={:#x}\n      mapa={}",
                si.si_code,
                unsafe { si.si_addr() } as usize,
                regs.rip,
                regs.rdi,
                regs.rsi,
                regs.rdx,
                regs.rsp,
                regs.rbp,
                regs.rax,
                regs.rbx,
                mapa.as_deref().unwrap_or("?")
            );
        }
        if sig == libc::SIGSEGV && regs.rip < 0x1000 {
            // retornamos para 0: é o fim da nossa chamada
            let rax = regs.rax;
            setar_regs(pid, &orig)?;
            return Ok(rax);
        }
        if sig == libc::SIGTRAP && regs.rip < 0x1000 {
            let rax = regs.rax;
            setar_regs(pid, &orig)?;
            return Ok(rax);
        }
        // outro sinal: repassa ao alvo e continua esperando
        let _ = unsafe {
            libc::ptrace(libc::PTRACE_CONT, pid, ptr::null_mut::<c_void>(), sig as usize as *mut c_void)
        };
    }
}
