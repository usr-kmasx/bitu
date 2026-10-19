//! Hook para anexo a processo VIVO (via bitu-inject): breakpoint `int3` no
//! primeiro byte das funções de present + handler de SIGTRAP.
//!
//! Por que breakpoint e não salto de 12 bytes? Escrever 1 byte é atômico e não
//! depende de decodificar o prólogo da função (sem risco de cortar instrução).
//! Cada present custa 2 traps (int3 + single-step) — µs, nada a 60–240 fps.
//!
//! Fluxo por chamada:
//!   int3 → handler: restaura o byte original, marca TF (single-step) e faz o
//!   pacing; a instrução original executa; trap de single-step → handler
//!   recoloca o int3 e limpa TF.
//!
//! Semântica REAL dos traps no Linux x86-64 (validada por core dump; não
//! confiar em si_code/si_addr!):
//!  - `int3` de userspace entrega SIGTRAP com si_code = SI_KERNEL (0x80) e
//!    si_addr = 0 — NÃO TRAP_BRKPT com o endereço. O trap se identifica pelo
//!    RIP do ucontext: int3 é um trap de 1 byte, o RIP já avançou →
//!    RIP == endereço_do_hook + 1.
//!  - O trap de single-step (TF) também chega como SIGTRAP em formato que
//!    varia; a identificação é pelo thread-local PASSO (endereço cujo byte
//!    está temporariamente restaurado), nunca por si_code.
//!
//! Quando a GUI "desliga" o limite ela manda alvo 0; estes hooks continuam
//! instalados (bytes na memória), mas ficam inertes junto com o pacer.

use crate::pacing;
use std::cell::Cell;
use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

const TF: i64 = 0x100;
const REG_RIP: usize = 16;
const REG_EFL: usize = 17;

struct Alvo {
    addr: AtomicUsize, // 0 = slot livre
    orig: AtomicUsize, // byte original (u8)
}

impl Alvo {
    const fn novo() -> Self {
        Alvo {
            addr: AtomicUsize::new(0),
            orig: AtomicUsize::new(0),
        }
    }
}

static ALVOS: [Alvo; 16] = [
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
    Alvo::novo(),
];
static INSTALADO: AtomicBool = AtomicBool::new(false);
static MASK: AtomicU32 = AtomicU32::new(0);
/// A sonda que resolve o present no ICD já foi disparada?
static SONDA: AtomicBool = AtomicBool::new(false);

static mut ANTIGO: libc::sigaction = unsafe { std::mem::zeroed() };

thread_local! {
    /// endereço cujo byte está temporariamente restaurado p/ single-step
    static PASSO: Cell<usize> = const { Cell::new(0) };
}

/// Chamado pelo injetor logo após o dlopen. Idempotente.
/// Retorna bitmask: 1 = GLX, 2 = EGL, 4 = Vulkan.
#[no_mangle]
pub unsafe extern "C" fn bitu_inject_install() -> u32 {
    if INSTALADO.swap(true, Ordering::SeqCst) {
        return MASK.load(Ordering::SeqCst);
    }

    // Já ativa pela via normal (manifest implícito/preload)? Os hooks de
    // interposição já contam cada present; int3 em cima contaria 2× e o
    // pacing esperaria dois períodos por frame.
    if pacing::ja_iniciado() {
        MASK.store(7, Ordering::SeqCst);
        return 7;
    }

    let mut mask = 0u32;
    // O handler de SIGTRAP PRECISA estar no lugar antes do primeiro int3:
    // o app apresenta continuamente e um trap sem handler mata o processo.
    instalar_handler();
    // Pré-aquece o que o handler usa ANTES de gravar qualquer int3: o TLS de
    // PASSO (materializa o bloco dinâmico desta thread) e o Pacer (env vars,
    // CString, OnceLock). malloc/TLS-de-primeiro-acesso dentro do handler não
    // são async-signal-safe — fora dele, sem risco.
    PASSO.with(|c| c.set(0));
    pacing::pre_aquecer();
    // GL/EGL: o app chama a função exportada, então hookar o símbolo resolve.
    // Vulkan é diferente: DXVK/winevulkan pegam o ponteiro por
    // vkGetDeviceProcAddr, que devolve um trampolim do loader — não o export
    // que o linker usa. O int3 precisa ir na implementação REAL do ICD. Essa
    // resolução acontece numa thread própria (ver sonda_vulkan): criar uma
    // instância Vulkan no thread sequestrado pelo ptrace poderia travar em
    // lock do driver.
    for (simbolo, libs, bit) in [
        ("glXSwapBuffers", &["libGL.so.1", "libGLX.so.0", "libGLdispatch.so.0"][..], 1u32),
        ("eglSwapBuffers", &["libEGL.so.1", "libGL.so.1"][..], 2),
    ] {
        if let Some(addr) = achar_simbolo(simbolo, libs) {
            if instalar_alvo(addr) {
                mask |= bit;
            }
        }
    }
    // Bit 4 otimista: a sonda instala o hook em ~200 ms. Se ela falhar, o
    // gráfico apenas não recebe frames (o BITU_DEBUG conta o motivo).
    let vk = vulkan_carregado();
    if vk {
        mask |= 4;
    }
    MASK.store(mask, Ordering::SeqCst);
    if vk && !SONDA.swap(true, Ordering::SeqCst) {
        if debug() {
            eprintln!("[bitu-inj] agendando sonda vulkan");
        }
        match std::thread::Builder::new()
            .name("bitu-sonda".into())
            .stack_size(256 * 1024)
            .spawn(|| unsafe { sonda_vulkan() })
        {
            Ok(_) => {}
            Err(e) => {
                if debug() {
                    eprintln!("[bitu-inj] falha ao criar thread da sonda: {e}");
                }
            }
        }
    }
    mask
}

/// Escreve o int3 no primeiro byte (página vira RWX; fica assim — necessário
/// para o handler restaurar/repor o byte durante o single-step).
unsafe fn instalar_alvo(addr: usize) -> bool {
    for a in ALVOS.iter() {
        if a.addr.load(Ordering::Acquire) == 0 {
            let pagina = addr & !0xfff;
            if libc::mprotect(
                pagina as *mut c_void,
                0x1000,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            ) != 0
            {
                if debug() {
                    eprintln!(
                        "[bitu-inj] mprotect falhou em {addr:#x}: {}",
                        std::io::Error::last_os_error()
                    );
                }
                return false;
            }
            let byte = std::ptr::read_volatile(addr as *const u8);
            if byte == 0xCC {
                return false; // já hookado por overlay/debugger: não mexe
            }
            a.orig.store(byte as usize, Ordering::Release);
            // Registra o slot ANTES de gravar o int3: outra thread do app pode
            // apresentar entre a escrita e o store. Com o slot já visível, o
            // handler reconhece o trap; sem ele, o SIGTRAP seria encaminhado
            // ao handler antigo (morte do processo).
            a.addr.store(addr, Ordering::Release);
            std::ptr::write_volatile(addr as *mut u8, 0xCC);
            let depois = std::ptr::read_volatile(addr as *const u8);
            if debug() {
                eprintln!(
                    "[bitu-inj] hook em {addr:#x} (byte {byte:#04x} -> {depois:#04x}){}",
                    if depois == 0xCC { "" } else { " — NÃO GRAVOU" }
                );
                if let Some(m) = mapa_de(addr) {
                    eprintln!("[bitu-inj]   mapa: {m}");
                }
            }
            return true;
        }
    }
    false
}

fn debug() -> bool {
    std::env::var("BITU_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Linha do /proc/self/maps que contém `addr` (só para diagnóstico).
fn mapa_de(addr: usize) -> Option<String> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    for l in maps.lines() {
        let range = l.split_whitespace().next()?;
        let mut it = range.split('-');
        let a = u64::from_str_radix(it.next()?, 16).ok()? as usize;
        let b = u64::from_str_radix(it.next()?, 16).ok()? as usize;
        if a <= addr && addr < b {
            return Some(l.to_string());
        }
    }
    None
}

unsafe fn instalar_handler() {
    let mut sa: libc::sigaction = std::mem::zeroed();
    sa.sa_sigaction = ao_trap as *const () as usize;
    sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
    libc::sigemptyset(&mut sa.sa_mask);
    libc::sigaction(
        libc::SIGTRAP,
        &sa,
        std::ptr::addr_of_mut!(ANTIGO),
    );
}

unsafe extern "C" fn ao_trap(sig: c_int, info: *mut libc::siginfo_t, uc: *mut c_void) {
    let gregs = &mut (*(uc as *mut libc::ucontext_t)).uc_mcontext.gregs;

    // 1) conclusão do nosso single-step: PASSO != 0 só existe entre o int3 e o
    //    single-step que o segue — nenhum código do app roda nesse intervalo.
    let passo = PASSO.with(|c| c.replace(0));
    if passo != 0 {
        // recoloca o int3 e limpa TF
        std::ptr::write_volatile(passo as *mut u8, 0xCC);
        gregs[REG_EFL] &= !TF;
        return;
    }

    // 2) int3 do nosso hook. Kernel x86-64 atual: SIGTRAP com si_code =
    //    SI_KERNEL (0x80) e si_addr = 0. O endereço do trap vem do RIP do
    //    ucontext (int3 avança 1 byte). si_addr entra como robustez para
    //    kernels que o preencham.
    let si = (*info).si_addr() as usize;
    let alvo = (gregs[REG_RIP] as usize).wrapping_sub(1);
    for a in ALVOS.iter() {
        let addr = a.addr.load(Ordering::Acquire);
        if addr == 0 || (addr != alvo && addr != si) {
            continue; // slot livre (sentinela 0) nunca casa
        }
        // confere que é o NOSSO int3 antes de qualquer escrita (nada cego)
        if std::ptr::read_volatile(addr as *const u8) != 0xCC {
            continue;
        }
        // restaura o byte original, RIP no início, TF ligado
        let orig = a.orig.load(Ordering::Acquire) as u8;
        std::ptr::write_volatile(addr as *mut u8, orig);
        gregs[REG_RIP] = addr as i64;
        gregs[REG_EFL] |= TF;
        PASSO.with(|c| c.set(addr));
        // conta o frame (e dorme o necessário) AQUI, antes do present real
        pacing::frame();
        return;
    }

    // SIGTRAP de terceiros (overlay/debugger): encaminha para o handler anterior
    let antigo = &*std::ptr::addr_of!(ANTIGO);
    let f = antigo.sa_sigaction;
    if f == 0 {
        // SIG_DFL: dispara o comportamento padrão (mata o processo)
        libc::signal(libc::SIGTRAP, libc::SIG_DFL);
        libc::raise(libc::SIGTRAP);
    } else if f != libc::SIG_IGN as usize {
        if antigo.sa_flags & libc::SA_SIGINFO != 0 {
            let h: unsafe extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) =
                std::mem::transmute(f);
            h(sig, info, uc);
        } else {
            let h: unsafe extern "C" fn(c_int) = std::mem::transmute(f);
            h(sig);
        }
    }
}

// ------------------------------------------------- Vulkan real (ICD/DXVK)
//
// Por que não hookar o export `vkQueuePresentKHR` do loader? Apps nativos que
// linkam libvulkan chamam esse export; mas Wine/DXVK pedem o ponteiro por
// vkGetDeviceProcAddr, que devolve outro trampolim do loader. Os dois caminhos
// terminam na entrada da dispatch table da fila; hookar essa entrada cobre
// ambos (com ou sem camadas no topo da cadeia).

/// O processo tem o loader Vulkan mapeado?
fn vulkan_carregado() -> bool {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return false;
    };
    maps.lines().any(|l| {
        l.split_whitespace()
            .nth(5)
            .and_then(|p| std::path::Path::new(p).file_name())
            .map(|f| f.to_string_lossy().starts_with("libvulkan.so"))
            .unwrap_or(false)
    })
}

/// Descobre o present REAL que o loader chama, criando uma instância/device
/// Vulkan mínimos pelo próprio loader (o mesmo caminho do jogo).
///
/// Por que não `vk_icdGetInstanceProcAddr` direto? Ele devolve um stub genérico
/// (sem WSI) que o loader NÃO usa. Com `VK_KHR_surface` + extensão de
/// plataforma na instância e `VK_KHR_swapchain` no device, a entrada da
/// dispatch table da fila aponta para a função WSI real — a mesma que o export
/// do loader e o trampolim de `vkGetDeviceProcAddr` chamam.
unsafe fn presents_vulkan() -> Vec<usize> {
    #[repr(C)]
    struct AppInfo {
        s_type: i32,
        p_next: *const c_void,
        p_app: *const std::os::raw::c_char,
        app_version: u32,
        p_engine: *const std::os::raw::c_char,
        engine_version: u32,
        api_version: u32,
    }
    #[repr(C)]
    struct InstanceInfo {
        s_type: i32,
        p_next: *const c_void,
        flags: u32,
        p_app: *const AppInfo,
        layer_count: u32,
        layers: *const *const std::os::raw::c_char,
        ext_count: u32,
        exts: *const *const std::os::raw::c_char,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct QueueFamily {
        flags: u32,
        queue_count: u32,
        timestamp_bits: u32,
        min_transfer: [u32; 3],
    }
    #[repr(C)]
    struct QueueInfo {
        s_type: i32,
        p_next: *const c_void,
        flags: u32,
        family: u32,
        count: u32,
        priorities: *const f32,
    }
    #[repr(C)]
    struct DeviceInfo {
        s_type: i32,
        p_next: *const c_void,
        flags: u32,
        queue_count: u32,
        queues: *const QueueInfo,
        layer_count: u32,
        layers: *const *const std::os::raw::c_char,
        ext_count: u32,
        exts: *const *const std::os::raw::c_char,
        features: *const c_void,
    }
    type CreateInstance =
        unsafe extern "system" fn(*const InstanceInfo, *const c_void, *mut *mut c_void) -> i32;
    type DestroyInstance = unsafe extern "system" fn(*mut c_void, *const c_void);
    type EnumPhys = unsafe extern "system" fn(*mut c_void, *mut u32, *mut *mut c_void) -> i32;
    type GetFamilies = unsafe extern "system" fn(*mut c_void, *mut u32, *mut QueueFamily);
    type CreateDevice = unsafe extern "system" fn(
        *mut c_void,
        *const DeviceInfo,
        *const c_void,
        *mut *mut c_void,
    ) -> i32;
    type DestroyDevice = unsafe extern "system" fn(*mut c_void, *const c_void);
    type GetQueue = unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void);

    const VK_SUCCESS: i32 = 0;
    const VK_STRUCTURE_TYPE_APPLICATION_INFO: i32 = 0;
    const VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO: i32 = 1;
    const VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO: i32 = 2;
    const VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO: i32 = 3;
    const VK_API_VERSION_1_0: u32 = 1 << 22;
    const LOADER_TABLE_MAGIC: u64 = 0x10aded040410aded;

    // Loader já mapeado: caminho exato do maps (sandbox) ou soname.
    let caminho = std::fs::read_to_string("/proc/self/maps").ok().and_then(|m| {
        m.lines().find_map(|l| {
            let p = l.split_whitespace().nth(5)?;
            let p = p.strip_suffix(" (deleted)").unwrap_or(p);
            let nome = std::path::Path::new(p)
                .file_name()?
                .to_string_lossy()
                .into_owned();
            if nome.starts_with("libvulkan.so") {
                Some(p.to_string())
            } else {
                None
            }
        })
    });
    let mut h = std::ptr::null_mut();
    if let Some(p) = caminho.as_deref() {
        if let Ok(c) = std::ffi::CString::new(p) {
            h = libc::dlopen(c.as_ptr(), libc::RTLD_NOW | 0x0004);
        }
    }
    if h.is_null() {
        h = libc::dlopen(c"libvulkan.so.1".as_ptr(), libc::RTLD_NOW | 0x0004);
    }
    if h.is_null() {
        if debug() {
            eprintln!("[bitu-inj] sonda: loader Vulkan não encontrado");
        }
        return Vec::new();
    }

    unsafe fn sym<T: Copy>(h: *mut c_void, n: &std::ffi::CStr) -> Option<T> {
        let p = libc::dlsym(h, n.as_ptr());
        if p.is_null() {
            None
        } else {
            Some(std::mem::transmute_copy(&p))
        }
    }

    let Some(create_inst) = sym::<CreateInstance>(h, c"vkCreateInstance") else {
        return Vec::new();
    };
    let Some(destroy_inst) = sym::<DestroyInstance>(h, c"vkDestroyInstance") else {
        return Vec::new();
    };
    let Some(enum_phys) = sym::<EnumPhys>(h, c"vkEnumeratePhysicalDevices") else {
        return Vec::new();
    };
    let Some(get_fams) = sym::<GetFamilies>(h, c"vkGetPhysicalDeviceQueueFamilyProperties") else {
        return Vec::new();
    };
    let Some(create_dev) = sym::<CreateDevice>(h, c"vkCreateDevice") else {
        return Vec::new();
    };
    let Some(destroy_dev) = sym::<DestroyDevice>(h, c"vkDestroyDevice") else {
        return Vec::new();
    };
    let Some(get_queue) = sym::<GetQueue>(h, c"vkGetDeviceQueue") else {
        return Vec::new();
    };
    let Some(export) = sym::<*mut c_void>(h, c"vkQueuePresentKHR") else {
        return Vec::new();
    };
    let Some(disp) = offset_present_loader(export) else {
        if debug() {
            eprintln!("[bitu-inj] sonda: não decodifiquei o salto do export do loader");
        }
        return Vec::new();
    };
    if debug() {
        eprintln!("[bitu-inj] sonda: loader {:?} disp={disp:#x}", caminho);
    }

    let app = AppInfo {
        s_type: VK_STRUCTURE_TYPE_APPLICATION_INFO,
        p_next: std::ptr::null(),
        p_app: std::ptr::null(),
        app_version: 0,
        p_engine: std::ptr::null(),
        engine_version: 0,
        api_version: VK_API_VERSION_1_0,
    };
    let mut out: Vec<usize> = Vec::new();
    // Bitu roda em X11/Xwayland e Wine costuma usar xlib ou xcb. Testar as
    // duas garante a função WSI certa; wayland fica como bônus.
    let plataformas = [
        ["VK_KHR_surface", "VK_KHR_xlib_surface"],
        ["VK_KHR_surface", "VK_KHR_xcb_surface"],
        ["VK_KHR_surface", "VK_KHR_wayland_surface"],
    ];
    for exts in plataformas {
        let nomes: Vec<std::ffi::CString> = exts
            .iter()
            .filter_map(|s| std::ffi::CString::new(*s).ok())
            .collect();
        if nomes.len() != exts.len() {
            continue;
        }
        let ptrs: Vec<*const std::os::raw::c_char> = nomes.iter().map(|c| c.as_ptr()).collect();
        let ci = InstanceInfo {
            s_type: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            p_app: &app,
            layer_count: 0,
            layers: std::ptr::null(),
            ext_count: ptrs.len() as u32,
            exts: ptrs.as_ptr(),
        };
        let mut inst: *mut c_void = std::ptr::null_mut();
        let r = create_inst(&ci, std::ptr::null(), &mut inst);
        if r != VK_SUCCESS || inst.is_null() {
            if debug() {
                eprintln!("[bitu-inj] sonda: instância {} -> {r}", exts[1]);
            }
            continue;
        }

        let mut n: u32 = 0;
        enum_phys(inst, &mut n, std::ptr::null_mut());
        let mut pdevs: Vec<*mut c_void> = Vec::new();
        if n > 0 {
            let cap = n.min(64);
            pdevs = vec![std::ptr::null_mut(); cap as usize];
            let mut m = cap;
            if enum_phys(inst, &mut m, pdevs.as_mut_ptr()) != VK_SUCCESS {
                pdevs.clear();
            } else {
                pdevs.truncate(m as usize);
            }
        }

        for pdev in pdevs {
            let mut nf: u32 = 0;
            get_fams(pdev, &mut nf, std::ptr::null_mut());
            if nf == 0 {
                continue;
            }
            let cap = nf.min(64);
            let mut fams: Vec<QueueFamily> = vec![QueueFamily::default(); cap as usize];
            let mut m = cap;
            get_fams(pdev, &mut m, fams.as_mut_ptr());
            fams.truncate(m as usize);
            let Some(fam) = fams
                .iter()
                .position(|f| f.queue_count > 0 && (f.flags & 1) != 0)
                .or_else(|| fams.iter().position(|f| f.queue_count > 0))
            else {
                continue;
            };
            let prio = 1.0f32;
            let qi = QueueInfo {
                s_type: VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                p_next: std::ptr::null(),
                flags: 0,
                family: fam as u32,
                count: 1,
                priorities: &prio,
            };
            let dexts = [c"VK_KHR_swapchain".as_ptr()];
            let di = DeviceInfo {
                s_type: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                p_next: std::ptr::null(),
                flags: 0,
                queue_count: 1,
                queues: &qi,
                layer_count: 0,
                layers: std::ptr::null(),
                ext_count: 1,
                exts: dexts.as_ptr(),
                features: std::ptr::null(),
            };
            let mut dev: *mut c_void = std::ptr::null_mut();
            if create_dev(pdev, &di, std::ptr::null(), &mut dev) != VK_SUCCESS || dev.is_null() {
                continue;
            }
            let mut q: *mut c_void = std::ptr::null_mut();
            get_queue(dev, fam as u32, 0, &mut q);
            if !q.is_null() {
                let tab = std::ptr::read_unaligned(q as *const usize);
                if tab != 0
                    && std::ptr::read_unaligned(tab as *const u64) == LOADER_TABLE_MAGIC
                {
                    let entry = std::ptr::read_unaligned((tab + disp) as *const usize);
                    if entry != 0 && !er_nosso(entry) && !out.contains(&entry) {
                        if debug() {
                            eprintln!(
                                "[bitu-inj] sonda: present {} = {entry:#x} ({})",
                                exts[1],
                                mapa_de(entry).as_deref().unwrap_or("?")
                            );
                        }
                        out.push(entry);
                    }
                }
            }
            destroy_dev(dev, std::ptr::null());
        }
        destroy_inst(inst, std::ptr::null());
    }
    out
}

/// Lê o deslocamento do salto `jmp *disp(%rax)` do export do loader. O loader
/// gera esse trampolim por versão; ler o disp evita hardcode (o do host é 0x690).
unsafe fn offset_present_loader(export: *mut c_void) -> Option<usize> {
    let code = std::slice::from_raw_parts(export as *const u8, 64);
    let mut i = 0;
    while i + 2 < code.len() {
        if code[i] == 0xff && code[i + 1] == 0xa0 && i + 6 <= code.len() {
            let d = i32::from_le_bytes([code[i + 2], code[i + 3], code[i + 4], code[i + 5]]);
            if d > 0 && d < 0x10000 {
                return Some(d as usize);
            }
        }
        if code[i] == 0xff && code[i + 1] == 0x60 {
            let d = code[i + 2] as i8 as isize;
            if d > 0 && d < 0x10000 {
                return Some(d as usize);
            }
        }
        i += 1;
    }
    None
}

/// Espera o injetor detachar, resolve o present no ICD e grava o int3.
/// Roda numa thread do próprio alvo: o thread principal pode estar parado no
/// ptrace, e chamar Vulkan nele poderia travar em lock do driver.
unsafe fn sonda_vulkan() {
    if debug() {
        eprintln!("[bitu-inj] sonda: thread iniciada");
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    if debug() {
        eprintln!("[bitu-inj] sonda: acordou");
    }
    // Não usar pacing::ja_iniciado() aqui: o próprio install já pré-aqueceu o
    // Pacer (PACER setado). A checagem de "layer já ativa" foi feita antes.
    let mut ok = false;
    for addr in presents_vulkan() {
        if instalar_alvo(addr) {
            ok = true;
        }
    }
    if !ok {
        // Sem ICD reconhecido: mantém o comportamento antigo (apps nativos que
        // chamam o export do loader diretamente).
        if let Some(addr) = achar_simbolo("vkQueuePresentKHR", &["libvulkan.so.1"]) {
            ok = instalar_alvo(addr);
        }
    }
    if ok {
        MASK.fetch_or(4, Ordering::SeqCst);
    }
    if debug() {
        eprintln!(
            "[bitu-inj] sonda vulkan: {}",
            if ok {
                "hook instalado"
            } else {
                "nenhum present encontrado"
            }
        );
    }
}

// ------------------------------------------- descoberta de símbolo no processo

/// Acha o endereço real de `simbolo` numa das libs candidatas.
/// 1º dlopen(RTLD_NOLOAD)+dlsym por lib (pega a função REAL, ignorando escopo
/// e evitando casar com os nossos próprios exports); 2º dlsym global se não
/// estiver no mapa; 3º leitura de /proc/self/maps + dynsym do arquivo.
unsafe fn achar_simbolo(simbolo: &str, libs: &[&str]) -> Option<usize> {
    let nome = std::ffi::CString::new(simbolo).ok()?;

    for lib in libs {
        let Ok(lib_c) = std::ffi::CString::new(*lib) else {
            continue;
        };
        // RTLD_NOW | RTLD_NOLOAD: só resolve se já estiver carregada
        let h = libc::dlopen(lib_c.as_ptr(), libc::RTLD_NOW | 0x0004);
        if h.is_null() {
            continue;
        }
        let p = libc::dlsym(h, nome.as_ptr());
        libc::dlclose(h);
        if !p.is_null() && !er_nosso(p as usize) {
            if debug() {
                eprintln!("[bitu-inj] {simbolo} via {lib} = {:#x}", p as usize);
            }
            return Some(p as usize);
        }
    }

    let p = libc::dlsym(libc::RTLD_DEFAULT, nome.as_ptr());
    if !p.is_null() && !er_nosso(p as usize) {
        if debug() {
            eprintln!("[bitu-inj] {simbolo} via dlsym = {:#x}", p as usize);
        }
        return Some(p as usize);
    }

    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    for linha in maps.lines() {
        let mut it = linha.split_whitespace();
        let range = it.next()?;
        let perms = it.next()?;
        if !perms.contains('x') {
            continue;
        }
        let _offset = it.next()?;
        let _dev = it.next()?;
        let _ino = it.next()?;
        let path = it.next().unwrap_or("");
        let arquivo = std::path::Path::new(path).file_name()?;
        let arquivo = arquivo.to_string_lossy();
        if !libs.iter().any(|l| arquivo == *l) {
            continue;
        }
        // base = início do mapa executável menos offset do arquivo
        let mut rr = range.split('-');
        let a = u64::from_str_radix(rr.next()?, 16).ok()?;
        let off = u64::from_str_radix(_offset, 16).ok()?;
        let base = a.saturating_sub(off);
        if let Some(v) = dynsym_do_arquivo(std::path::Path::new(path), simbolo) {
            if debug() {
                eprintln!("[bitu-inj] {simbolo} via {path} = {:#x}", base + v);
            }
            return Some((base + v) as usize);
        }
    }
    None
}

/// Endereço pertence às nossas próprias páginas (libbitu_layer.so)?
fn er_nosso(addr: usize) -> bool {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return false;
    };
    for l in maps.lines() {
        let Some(range) = l.split_whitespace().next() else {
            continue;
        };
        let mut it = range.split('-');
        let (Some(a), Some(b)) = (it.next(), it.next()) else {
            continue;
        };
        let (Ok(a), Ok(b)) = (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16)) else {
            continue;
        };
        if (a as usize) <= addr && addr < b as usize {
            return l.ends_with("libbitu_layer.so");
        }
    }
    false
}

/// st_value do símbolo na dynsym do ELF64 LE (offset relativo à base).
fn dynsym_do_arquivo(path: &std::path::Path, procurado: &str) -> Option<u64> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 64 || &data[..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return None;
    }
    let shoff = get_u64(&data, 0x28)? as usize;
    let shentsize = get_u16(&data, 0x3a)?;
    let shnum = get_u16(&data, 0x3c)?;
    for i in 0..shnum {
        let sh = shoff + i * shentsize;
        if get_u32(&data, sh + 4)? != 11 {
            continue; // SHT_DYNSYM
        }
        let sh_offset = get_u64(&data, sh + 0x18)? as usize;
        let sh_size = get_u64(&data, sh + 0x20)? as usize;
        let sh_link = get_u32(&data, sh + 0x28)? as usize;
        let str_hdr = shoff + sh_link * shentsize;
        let stroff = get_u64(&data, str_hdr + 0x18)? as usize;
        for s in 0..(sh_size / 24) {
            let so = sh_offset + s * 24;
            let st_name = get_u32(&data, so)? as usize;
            let st_shndx = get_u16(&data, so + 6)?;
            let st_value = get_u64(&data, so + 8)?;
            if st_name == 0 || st_shndx == 0 || st_value == 0 {
                continue;
            }
            let off = stroff + st_name;
            if off >= data.len() {
                continue;
            }
            let fim = data[off..].iter().position(|&b| b == 0).map(|p| off + p)?;
            if data.get(off..fim) == Some(procurado.as_bytes()) {
                return Some(st_value);
            }
        }
    }
    None
}

fn get_u16(d: &[u8], o: usize) -> Option<usize> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?) as usize)
}

fn get_u32(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

fn get_u64(d: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(o..o + 8)?.try_into().ok()?))
}
