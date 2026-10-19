//! Bitu — limitador de frame time (30 fps / 33,33 ms) para Linux.
//! GUI en-US, janela fixa 640x180 (32:9): três cartões no topo (título,
//! telemetria, alvo) e o gráfico embaixo. O botão de controle abre o popup
//! de PIDs para anexar a layer; o alvo é sempre 30 fps.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::collections::VecDeque;
use std::os::raw::{c_int, c_void};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RETENCAO: usize = 4096;
const PONTOS_GRAFICO: usize = 600;
/// Janela das médias (fps/FT médio) = mesma do gráfico (~20 s a 30 fps).
const JANELA_STATS: usize = PONTOS_GRAFICO;

/// Alvo fixo do app: 30 fps.
const ALVO_US: u32 = 33_333;
/// Janela fixa 32:9, sem redimensionar nem maximizar.
const JANELA_L: f32 = 640.0;
const JANELA_A: f32 = 180.0;

// Cores do gráfico: verde = no alvo; amarelo = desvio 1–3 ms sem causa
// definida; laranja = atraso do jogo; vermelho = atraso do pacer.
const COR_VERDE: egui::Color32 = egui::Color32::from_rgb(70, 230, 110);
const COR_AMARELO: egui::Color32 = egui::Color32::from_rgb(250, 200, 40);
const COR_LARANJA: egui::Color32 = egui::Color32::from_rgb(255, 145, 55);
const COR_VERMELHO: egui::Color32 = egui::Color32::from_rgb(235, 80, 70);
/// Cartões cinza sobre o fundo preto (gráfico incluso).
const COR_CARTAO: egui::Color32 = egui::Color32::from_rgb(38, 38, 42);
const COR_TEXTO_SUAVE: egui::Color32 = egui::Color32::from_rgb(168, 170, 176);

// ---------------------------------------------------------------- estado

struct Compartilhado {
    /// (pid do peer, frame time em µs, atraso total em µs, atraso de wake
    /// do pacer em µs, alvo usado no frame em µs; u32::MAX = layer antiga).
    /// O atraso total é o quanto a liberação passou do deadline.
    samples_us: Mutex<VecDeque<(i32, u32, u32, u32, u32)>>,
    frames_totais: Mutex<u64>,
    /// fps alvo enviado por último para a layer (µs); 0 = sem limite
    alvo_us: Mutex<u32>,
    /// pid do app selecionado para limitar (0 = nenhum); lido pela thread IPC
    alvo_pid: Mutex<i32>,
    conns: Mutex<Vec<i32>>,
    pids: Mutex<Vec<(i32, i32)>>, // (fd, pid do peer)
}

impl Compartilhado {
    fn new() -> Self {
        Compartilhado {
            samples_us: Mutex::new(VecDeque::new()),
            frames_totais: Mutex::new(0),
            alvo_us: Mutex::new(0),
            alvo_pid: Mutex::new(0),
            conns: Mutex::new(Vec::new()),
            pids: Mutex::new(Vec::new()),
        }
    }
}

/// Caminho da layer compartilhada, ao lado do binário da GUI.
fn lib_layer_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join("libbitu_layer.so");
    p.is_file().then_some(p)
}

fn sock_path() -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    let dir = std::path::PathBuf::from(base).join("bitu");
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join("bitu.sock"); // caminho fixo: declaração de launch-options estável
    let _ = std::fs::remove_file(&p);
    Some(p)
}

/// Socket ABSTRATO do Linux (`\0bitu-<uid>`): atravessa sandboxes
/// (Flatpak/pressure-vessel) que filtram o /run/user do processo.
fn sock_abstrato() -> Vec<u8> {
    let mut v = vec![0u8];
    v.extend_from_slice(format!("bitu-{}", unsafe { libc::getuid() }).as_bytes());
    v
}

// --------------------------------------------------------- processos + manifest

/// Processo do usuário (lista para anexar por PID).
struct ProcInfo {
    pid: i32,
    nome: String,
    cmd: String,
    conectado: bool,
}

/// Nome curto do processo (/proc/<pid>/comm).
fn proc_nome(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("pid {pid}"))
}

/// Linha de comando completa (/proc/<pid>/cmdline); vazia p/ kernel threads.
fn proc_cmd(pid: i32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            String::from_utf8_lossy(&b)
                .split('\0')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Todos os processos do usuário; conectados primeiro.
fn listar_processos(conectados: &[i32], meu_pid: i32) -> Vec<ProcInfo> {
    use std::os::unix::fs::MetadataExt;
    let uid = unsafe { libc::getuid() };
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if pid == meu_pid {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        if md.uid() != uid {
            continue;
        }
        let cmd = proc_cmd(pid);
        if cmd.is_empty() {
            continue; // kernel thread / zumbi
        }
        out.push(ProcInfo {
            pid,
            nome: proc_nome(pid),
            cmd,
            conectado: conectados.contains(&pid),
        });
    }
    out.sort_by(|a, b| {
        b.conectado
            .cmp(&a.conectado)
            .then_with(|| a.nome.to_lowercase().cmp(&b.nome.to_lowercase()))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    out
}

/// Caminho do injetor, ao lado do binário da GUI.
fn injetor_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join("bitu-inject");
    p.is_file().then_some(p)
}

// --------------------------------------------------------------- servidor IPC

extern "C" fn thread_servidor(raw: *mut c_void) -> *mut libc::c_void {
    let ctx = unsafe { Box::from_raw(raw as *mut CtxServidor) };
    ctx.loop_servidor();
    std::ptr::null_mut()
}

struct CtxServidor {
    compart: Arc<Compartilhado>,
    listeners: Vec<c_int>,
}

impl CtxServidor {
    fn loop_servidor(self) {
        let mut fds: Vec<libc::pollfd> = self
            .listeners
            .iter()
            .map(|&fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let n_list = fds.len();
        loop {
            for f in fds.iter_mut() {
                f.revents = 0;
            }
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 40) };
            if rc < 0 {
                continue;
            }
            // novas conexões (abstrato e/ou arquivo)
            for li in 0..n_list {
                if fds[li].revents & libc::POLLIN == 0 {
                    continue;
                }
                let mut addr: libc::sockaddr = unsafe { std::mem::zeroed() };
                let mut len: libc::socklen_t =
                    std::mem::size_of::<libc::sockaddr>() as libc::socklen_t;
                let nova = unsafe {
                    libc::accept(
                        fds[li].fd,
                        &mut addr as *mut _ as *mut libc::sockaddr,
                        &mut len,
                    )
                };
                if nova >= 0 {
                    fds.push(libc::pollfd {
                        fd: nova,
                        events: libc::POLLIN,
                        revents: 0,
                    });
                    if let Ok(mut m) = self.compart.conns.lock() {
                        m.push(nova);
                    }
                    // pid do peer (SO_PEERCRED)
                    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
                    let mut clen = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
                    unsafe {
                        libc::getsockopt(
                            nova,
                            libc::SOL_SOCKET,
                            libc::SO_PEERCRED,
                            &mut cred as *mut _ as *mut c_void,
                            &mut clen,
                        );
                    }
                    if let Ok(mut m) = self.compart.pids.lock() {
                        m.retain(|&(fd, _)| fd != nova);
                        m.push((nova, cred.pid));
                    }
                    // envia o alvo imediatamente — só se este pid for o selecionado
                    let sel = self.compart.alvo_pid.lock().map(|g| *g).unwrap_or(0);
                    let alvo = if cred.pid == sel {
                        self.compart.alvo_us.lock().map(|g| *g).unwrap_or(0)
                    } else {
                        0
                    };
                    let mut out = [0u8; 8];
                    out[..4].copy_from_slice(&alvo.to_le_bytes());
                    unsafe {
                        libc::send(nova, out.as_ptr() as *const c_void, 8, libc::MSG_NOSIGNAL);
                    }
                }
            }
            // lê amostras (frame time em µs)
            for i in (n_list..fds.len()).rev() {
                let fd = fds[i].fd;
                if fds[i].revents & libc::POLLIN == 0 {
                    continue;
                }
                // 20 bytes no protocolo novo (ft, contador, atraso, wake do
                // pacer, alvo); aceita 16, 12 e 8 de layers antigas para
                // atualizar GUI e layer em qualquer ordem (seqpacket descarta
                // o excedente).
                let mut buf = [0u8; 20];
                let n = unsafe {
                    libc::recv(fd, buf.as_mut_ptr() as *mut c_void, 20, libc::MSG_DONTWAIT)
                };
                if n == 8 || n == 12 || n == 16 || n == 20 {
                    let (ft_us, atraso_us, acordou_us, alvo_amostra_us) =
                        decodificar_amostra(&buf[..n as usize]).unwrap();
                    let pid = self
                        .compart
                        .pids
                        .lock()
                        .ok()
                        .and_then(|m| m.iter().find(|&&(f, _)| f == fd).map(|&(_, p)| p));
                    if let Some(pid) = pid {
                        if let Ok(mut q) = self.compart.samples_us.lock() {
                            q.push_back((pid, ft_us, atraso_us, acordou_us, alvo_amostra_us));
                            while q.len() > RETENCAO {
                                q.pop_front();
                            }
                        }
                        if let Ok(mut f) = self.compart.frames_totais.lock() {
                            *f += 1;
                        }
                    }
                } else if n <= 0 {
                    let err = std::io::Error::last_os_error().raw_os_error();
                    let dead = n == 0
                        || matches!(err, Some(libc::ECONNRESET) | Some(libc::ENOTCONN) | Some(libc::EPIPE));
                    if dead {
                        // pid do app deste fd, antes de limpar os mapas
                        let era_pid = self
                            .compart
                            .pids
                            .lock()
                            .ok()
                            .and_then(|m| m.iter().find(|&&(f, _)| f == fd).map(|&(_, p)| p));
                        unsafe {
                            libc::close(fd);
                        }
                        fds.remove(i);
                        if let Ok(mut m) = self.compart.conns.lock() {
                            m.retain(|&x| x != fd);
                        }
                        if let Ok(mut m) = self.compart.pids.lock() {
                            m.retain(|&(x, _)| x != fd);
                        }
                        // se o app selecionado saiu, solta a trava
                        if let Some(p) = era_pid {
                            if let Ok(mut g) = self.compart.alvo_pid.lock() {
                                if *g == p {
                                    *g = 0;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Abre e põe para escutar um endereço unix: nome abstrato (1º byte 0) ou
/// caminho de arquivo.
fn abrir_listener_unix(ala: &[u8]) -> Result<c_int, std::io::Error> {
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let limite = sa.sun_path.len();
    let len: libc::socklen_t = if ala.first() == Some(&0) {
        if ala.len() > limite {
            unsafe { libc::close(fd) };
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "nome abstrato do socket muito longo",
            ));
        }
        for (i, &b) in ala.iter().enumerate() {
            sa.sun_path[i] = b as libc::c_char;
        }
        // offsetof(sockaddr_un, sun_path) = 2; o abstrato inclui o NUL inicial
        (std::mem::size_of::<libc::sa_family_t>() + ala.len()) as libc::socklen_t
    } else {
        if ala.len() + 1 > limite {
            unsafe { libc::close(fd) };
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "caminho do socket muito longo",
            ));
        }
        for (i, &b) in ala.iter().enumerate() {
            sa.sun_path[i] = b as libc::c_char;
        }
        std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t
    };
    if unsafe { libc::bind(fd, &sa as *const _ as *const libc::sockaddr, len) } != 0 {
        let e = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    if unsafe { libc::listen(fd, 64) } != 0 {
        let e = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(fd)
}

/// Decodifica uma amostra da layer: 8 bytes (ft + contador), 12 bytes
/// (ft + contador + atraso), 16 bytes (+ wake do pacer) ou 20 bytes (+ alvo).
/// Devolve (ft_us, atraso_us, acordou_us, alvo_us); camadas antigas não
/// mandam o alvo, então ele fica `u32::MAX` (desconhecido).
fn decodificar_amostra(buf: &[u8]) -> Option<(u32, u32, u32, u32)> {
    if buf.len() < 8 {
        return None;
    }
    let ft_us = u32::from_le_bytes(buf[..4].try_into().ok()?);
    let atraso_us = if buf.len() >= 12 {
        u32::from_le_bytes(buf[8..12].try_into().ok()?)
    } else {
        0
    };
    let acordou_us = if buf.len() >= 16 {
        u32::from_le_bytes(buf[12..16].try_into().ok()?)
    } else {
        0
    };
    let alvo_us = if buf.len() >= 20 {
        u32::from_le_bytes(buf[16..20].try_into().ok()?)
    } else {
        u32::MAX
    };
    Some((ft_us, atraso_us, acordou_us, alvo_us))
}

/// Mostra só amostras do regime atual (mesmo alvo). Amostras de camada
/// antiga vêm com alvo desconhecido e continuam aparecendo em qualquer modo.
fn do_regime(alvo_amostra_us: u32, alvo_atual_us: u32) -> bool {
    alvo_amostra_us == u32::MAX || alvo_amostra_us == alvo_atual_us
}

fn iniciar_servidor(sock: std::path::PathBuf, compart: Arc<Compartilhado>) -> Result<(), std::io::Error> {
    let mut listeners = Vec::new();
    // abstrato primeiro: atravessa sandboxes (Flatpak/pressure-vessel) que
    // filtram o /run/user do processo
    match abrir_listener_unix(&sock_abstrato()) {
        Ok(fd) => listeners.push(fd),
        Err(e) => eprintln!("bitu: socket abstrato indisponível: {e}"),
    }
    // arquivo: compatibilidade com ferramentas/scripts e BITU_SOCK
    match abrir_listener_unix(sock.to_string_lossy().as_bytes()) {
        Ok(fd) => listeners.push(fd),
        Err(e) => eprintln!("bitu: socket de arquivo indisponível: {e}"),
    }
    if listeners.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "nenhum socket IPC pôde ser aberto",
        ));
    }

    let ctx = Box::into_raw(Box::new(CtxServidor { compart, listeners }));
    let mut tid: libc::pthread_t = 0;
    unsafe {
        libc::pthread_create(
            &mut tid as *mut _,
            std::ptr::null(),
            thread_servidor,
            ctx as *mut c_void,
        );
    }
    Ok(())
}

// --------------------------------------------------------------- eframe App

/// Lógica pura do gate por PID (testável headless): para cada conexão,
/// o fd do PID selecionado recebe o alvo atual; os demais recebem 0.
fn redistribuir_alvo(compart: &Compartilhado) {
    let sel = compart.alvo_pid.lock().map(|g| *g).unwrap_or(0);
    let us = compart.alvo_us.lock().map(|g| *g).unwrap_or(0);
    let pids = compart.pids.lock().map(|m| m.clone()).unwrap_or_default();
    let conns = compart.conns.lock().map(|m| m.clone()).unwrap_or_default();
    let mut ligado = [0u8; 8];
    ligado[..4].copy_from_slice(&us.to_le_bytes());
    let desligado = [0u8; 8];
    for &fd in conns.iter() {
        let pid = pids
            .iter()
            .find(|&&(f, _)| f == fd)
            .map(|&(_, p)| p)
            .unwrap_or(0);
        let buf = if sel != 0 && pid == sel {
            ligado.as_ptr()
        } else {
            desligado.as_ptr()
        };
        unsafe {
            libc::send(fd, buf as *const c_void, 8, libc::MSG_NOSIGNAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// socketpair() → (lado "client"-como-layer, lado "GUI")
    fn par() -> (i32, i32) {
        let mut fds = [0i32; 2];
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0);
        (fds[0], fds[1])
    }

    fn comp(comparar: (i32, i32), pids: Vec<(i32, i32)>, alvo_pid: i32, alvo_us: u32) -> Compartilhado {
        let c = Compartilhado::new();
        if let Ok(mut m) = c.conns.lock() {
            *m = vec![comparar.0, comparar.1];
        }
        if let Ok(mut m) = c.pids.lock() {
            *m = pids;
        }
        if let Ok(mut m) = c.alvo_pid.lock() {
            *m = alvo_pid;
        }
        if let Ok(mut m) = c.alvo_us.lock() {
            *m = alvo_us;
        }
        c
    }

    fn recv_u32(fd: i32) -> Option<u32> {
        let mut buf = [0u8; 8];
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut c_void, 8, libc::MSG_DONTWAIT) };
        if n == 8 {
            Some(u32::from_le_bytes(buf[..4].try_into().unwrap()))
        } else {
            None
        }
    }

    /// Apenas o fd do PID selecionado recebe o alvo; os outros recebem 0.
    #[test]
    fn gate_por_pid() {
        let (fd_a, gui_a) = par();
        let (fd_b, gui_b) = par();
        let c = comp((fd_a, fd_b), vec![(fd_a, 100), (fd_b, 200)], 100, 33_333);
        redistribuir_alvo(&c);
        assert_eq!(recv_u32(gui_a), Some(33_333), "selecionado recebe o alvo");
        assert_eq!(recv_u32(gui_b), Some(0), "não Selecionado recebe 0 (mede, não limita)");
        unsafe { libc::close(fd_a); libc::close(fd_b); libc::close(gui_a); libc::close(gui_b); }
    }

    /// Trocar a seleção muda os bytes na hora (mesma semântica do clique na UI).
    #[test]
    fn troca_de_sel() {
        let (fd_a, gui_a) = par();
        let (fd_b, gui_b) = par();
        let c = comp((fd_a, fd_b), vec![(fd_a, 100), (fd_b, 200)], 200, 16_667);
        redistribuir_alvo(&c);
        assert_eq!(recv_u32(gui_a), Some(0), "perdeu a seleção → 0");
        assert_eq!(recv_u32(gui_b), Some(16_667), "novo selecionado → alvo");
        unsafe { libc::close(fd_a); libc::close(fd_b); libc::close(gui_a); libc::close(gui_b); }
    }

    /// Sem seleção (0): ninguém é limitado — estado "desligar limite".
    #[test]
    fn sem_sel_ninguem_limitado() {
        let (fd_a, gui_a) = par();
        let (fd_b, gui_b) = par();
        let c = comp((fd_a, fd_b), vec![(fd_a, 100), (fd_b, 200)], 0, 16_667);
        redistribuir_alvo(&c);
        assert_eq!(recv_u32(gui_a), Some(0));
        assert_eq!(recv_u32(gui_b), Some(0));
        unsafe { libc::close(fd_a); libc::close(fd_b); libc::close(gui_a); libc::close(gui_b); }
    }

    /// pid não presente no mapa: tratado como não selecionado.
    #[test]
    fn fd_sem_pid_nunca_recebe_alvo() {
        let (fd_a, gui_a) = par();
        let c = comp((fd_a, -1), vec![], 100, 16_667);
        redistribuir_alvo(&c);
        assert_eq!(recv_u32(gui_a), Some(0));
        unsafe { libc::close(fd_a); libc::close(gui_a); }
    }

    /// Layer antiga (8/12/16 bytes) e nova (20 bytes) decodificam igual;
    /// cada campo só existe quando o protocolo o carrega. Camada antiga não
    /// manda alvo: fica `u32::MAX` (desconhecido).
    #[test]
    fn amostra_8_12_16_e_20_bytes() {
        let mut b8 = [0u8; 8];
        b8[..4].copy_from_slice(&33_333u32.to_le_bytes());
        b8[4..8].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(decodificar_amostra(&b8), Some((33_333, 0, 0, u32::MAX)));

        let mut b12 = [0u8; 12];
        b12[..4].copy_from_slice(&33_333u32.to_le_bytes());
        b12[4..8].copy_from_slice(&7u32.to_le_bytes());
        b12[8..12].copy_from_slice(&1_500u32.to_le_bytes());
        assert_eq!(decodificar_amostra(&b12), Some((33_333, 1_500, 0, u32::MAX)));

        let mut b16 = [0u8; 16];
        b16[..4].copy_from_slice(&33_333u32.to_le_bytes());
        b16[4..8].copy_from_slice(&7u32.to_le_bytes());
        b16[8..12].copy_from_slice(&1_500u32.to_le_bytes());
        b16[12..16].copy_from_slice(&900u32.to_le_bytes());
        assert_eq!(decodificar_amostra(&b16), Some((33_333, 1_500, 900, u32::MAX)));

        let mut b20 = [0u8; 20];
        b20[..4].copy_from_slice(&33_333u32.to_le_bytes());
        b20[4..8].copy_from_slice(&7u32.to_le_bytes());
        b20[8..12].copy_from_slice(&1_500u32.to_le_bytes());
        b20[12..16].copy_from_slice(&900u32.to_le_bytes());
        b20[16..20].copy_from_slice(&33_333u32.to_le_bytes());
        assert_eq!(decodificar_amostra(&b20), Some((33_333, 1_500, 900, 33_333)));

        assert_eq!(decodificar_amostra(&[0u8; 4]), None);
    }

    /// O gráfico/telemetria só mostram o regime atual: amostra de outro
    /// alvo (ou sem limite) fica fora; camada antiga (alvo desconhecido)
    /// continua aparecendo.
    #[test]
    fn regime_filtra_por_alvo() {
        assert!(do_regime(33_333, 33_333));
        assert!(do_regime(u32::MAX, 33_333));
        assert!(!do_regime(16_667, 33_333));
        assert!(!do_regime(0, 33_333));
        assert!(do_regime(0, 0));
        assert!(!do_regime(33_333, 0));
    }

    /// Médias do cartão: última amostra = fps/FT; as médias usam a janela de 600.
    #[test]
    fn estatisticas_da_janela() {
        let c = Compartilhado::new();
        {
            let mut q = c.samples_us.lock().unwrap();
            // 601ª amostra para trás (mais antiga): não entra na média
            q.push_back((100, 100_000, 0, 0, ALVO_US));
            for _ in 0..600 {
                q.push_back((100, 30_000, 0, 0, ALVO_US));
            }
            // outro PID e outro regime ficam de fora
            q.push_back((200, 16_000, 0, 0, ALVO_US));
            q.push_back((100, 16_000, 0, 0, 0));
        }
        let s = estatisticas(&c.samples_us, ALVO_US, 100).unwrap();
        assert!((s.ft_ms - 30.0).abs() < 1e-9);
        assert!((s.ft_medio_ms - 30.0).abs() < 1e-9, "média ignora fora da janela");
        assert!((s.fps - 1e6 / 30_000.0).abs() < 1e-6);
        assert!((s.fps_medio - 1e6 / 30_000.0).abs() < 1e-6);
        assert!(estatisticas(&c.samples_us, ALVO_US, 999).is_none());
    }

    /// Cores por causa: verde no alvo; amarelo desvio; laranja jogo; vermelho pacer.
    #[test]
    fn nivel_por_causa() {
        assert_eq!(nivel(33.3, 0, 0, 33.333), 0, "no alvo = verde");
        assert_eq!(nivel(35.0, 0, 0, 33.333), 1, "desvio 1–3 ms = amarelo");
        assert_eq!(nivel(37.0, 0, 0, 33.333), 2, "desvio >3 ms = laranja");
        assert_eq!(nivel(33.3, 2_000, 0, 33.333), 2, "atraso do jogo = laranja");
        assert_eq!(nivel(34.0, 2_400, 1_500, 33.333), 3, "wake do pacer = vermelho");
        assert_eq!(nivel(50.0, 5_000, 0, 0.0), 0, "sem alvo: sem violação");
    }

    /// Janela fixa em 32:9 (640x180).
    #[test]
    fn janela_32x9() {
        assert!((JANELA_L / JANELA_A - 32.0 / 9.0).abs() < 1e-6);
    }

    /// A linha do FT sai como polilinha contínua por trecho de cor: com
    /// dados planos, um único caminho de 600 pontos — e nenhum segmento por
    /// amostra (era o que deixava a linha com aparência tracejada sobre o alvo).
    #[test]
    fn linha_do_grafico_continua() {
        let c = Arc::new(Compartilhado::new());
        {
            let mut q = c.samples_us.lock().unwrap();
            for _ in 0..PONTOS_GRAFICO {
                q.push_back((42, ALVO_US, 0, 0, ALVO_US));
            }
        }
        if let Ok(mut g) = c.alvo_pid.lock() {
            *g = 42;
        }
        let mut b = Bitu::novo(c);
        let ctx = egui::Context::default();
        let entrada = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(JANELA_L, JANELA_A),
            )),
            ..Default::default()
        };
        let saida = ctx.run(entrada, |ctx| desenhar(ctx, &mut b));

        let mut polilinhas = 0usize;
        let mut segmentos = 0usize;
        for cs in &saida.shapes {
            match &cs.shape {
                egui::Shape::Path(p) if p.points.len() == PONTOS_GRAFICO => polilinhas += 1,
                egui::Shape::LineSegment { .. } => segmentos += 1,
                _ => {}
            }
        }
        assert_eq!(polilinhas, 1, "uma polilinha de 600 pontos");
        // só a linha do alvo (1) e o d-pad do ícone de controle (2) usam segmentos
        assert!(
            segmentos <= 3,
            "nenhum segmento por amostra (achei {segmentos})"
        );
    }

    /// Renderiza a GUI headless (640x180) e confere textos e cores do layout.
    #[test]
    fn layout_headless() {
        let mut b = Bitu::novo(Arc::new(Compartilhado::new()));
        let ctx = egui::Context::default();
        let entrada = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(JANELA_L, JANELA_A),
            )),
            ..Default::default()
        };
        let saida = ctx.run(entrada, |ctx| desenhar(ctx, &mut b));

        let mut textos: Vec<String> = Vec::new();
        let mut cartoes = 0usize;
        for cs in &saida.shapes {
            match &cs.shape {
                egui::Shape::Text(t) => {
                    let fim_x = t.pos.x + t.galley.size().x;
                    let fim_y = t.pos.y + t.galley.size().y;
                    assert!(
                        fim_x <= JANELA_L + 1.0 && fim_y <= JANELA_A + 1.0,
                        "texto cortado: {:?} até ({fim_x:.0},{fim_y:.0})",
                        t.galley.text()
                    );
                    textos.push(t.galley.text().to_string());
                }
                egui::Shape::Rect(r) if r.fill == COR_CARTAO => cartoes += 1,
                _ => {}
            }
        }
        let tem = |s: &str| textos.iter().any(|t| t.trim() == s);
        assert!(
            tem(&format!("Bitu v{}", env!("CARGO_PKG_VERSION"))),
            "título do cartão 1"
        );
        assert!(tem("frame limiter"));
        assert!(tem("fps") && tem("avg fps") && tem("FT") && tem("avg FT"));
        assert!(tem("frame time by app"));
        assert!(tem("33.33 ms (30 fps)"), "cartão do alvo fixo");
        assert!(cartoes >= 4, "3 cartões + gráfico em cinza (achei {cartoes})");
        assert!(
            !textos.iter().any(|t| t.contains("alvo") || t.contains('±')),
            "gráfico sem rótulos de texto"
        );

        // popup de PIDs também renderiza sem panic
        b.popup_aberto = true;
        let mut textos: Vec<String> = Vec::new();
        for _ in 0..2 {
            let entrada = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(JANELA_L, JANELA_A),
                )),
                ..Default::default()
            };
            let saida = ctx.run(entrada, |ctx| desenhar(ctx, &mut b));
            textos.clear();
            for cs in &saida.shapes {
                if let egui::Shape::Text(t) = &cs.shape {
                    let fim_x = t.pos.x + t.galley.size().x;
                    let fim_y = t.pos.y + t.galley.size().y;
                    assert!(
                        fim_x <= JANELA_L + 1.0 && fim_y <= JANELA_A + 1.0,
                        "texto do popup cortado: {:?} até ({fim_x:.0},{fim_y:.0})",
                        t.galley.text()
                    );
                    textos.push(t.galley.text().to_string());
                }
            }
        }
        let tem = |s: &str| textos.iter().any(|t| t.trim() == s);
        assert!(tem("attach to process"), "título do popup: {textos:?}");
        assert!(tem("filter"), "campo de filtro do popup");
    }
}

struct Bitu {
    compart: Arc<Compartilhado>,
    /// libbitu_layer.so ao lado do binário, injetada nos anexos por PID.
    lib_path: Option<std::path::PathBuf>,
    ult_msg: String,
    /// processos do usuário (cache, atualiza a cada 500 ms)
    lista: Vec<ProcInfo>,
    busca: String,
    prox_refresh: std::time::Instant,
    /// injeção em andamento: (pid alvo, resultado)
    injecao_rx: Option<std::sync::mpsc::Receiver<(i32, Result<u32, String>)>>,
    /// popup de PIDs aberto
    popup_aberto: bool,
    /// já registrou o tamanho efetivo da janela (uma vez por sessão)
    logou_tamanho: bool,
}

impl Bitu {
    fn novo(compart: Arc<Compartilhado>) -> Self {
        Bitu {
            compart,
            lib_path: lib_layer_path(),
            ult_msg: String::new(),
            lista: Vec::new(),
            busca: String::new(),
            prox_refresh: std::time::Instant::now(),
            injecao_rx: None,
            popup_aberto: false,
            logou_tamanho: false,
        }
    }

    /// Empurra o alvo fixo (30 fps) para o PID selecionado; os demais
    /// recebem 0 (a layer mede, mas não limita).
    fn enviar_alvo(&self) {
        redistribuir_alvo(&self.compart);
    }

    /// Marca (ou desmarca, pid 0) o app que será limitado.
    fn definir_sel(&mut self, pid: i32) {
        if let Ok(mut g) = self.compart.alvo_pid.lock() {
            *g = pid;
        }
        self.enviar_alvo();
        self.ult_msg = if pid == 0 {
            "limit off".to_string()
        } else {
            format!("limiting pid {pid}")
        };
    }

    /// Recarrega a lista de processos do usuário.
    fn atualizar_lista(&mut self) {
        let conectados: Vec<i32> = self
            .compart
            .pids
            .lock()
            .map(|m| m.iter().map(|&(_, p)| p).collect())
            .unwrap_or_default();
        let meu = std::process::id() as i32;
        self.lista = listar_processos(&conectados, meu);
        // processo selecionado saiu? solta a trava (a thread IPC também cuida disto)
        let sel = self.compart.alvo_pid.lock().map(|g| *g).unwrap_or(0);
        if sel != 0 && !self.lista.iter().any(|a| a.pid == sel) {
            if let Ok(mut g) = self.compart.alvo_pid.lock() {
                *g = 0;
            }
            self.enviar_alvo();
            self.ult_msg = format!("process {sel} exited");
        }
    }

    /// Injeta a layer no processo VIVO e já o marca como alvo do limite.
    fn anexar(&mut self, pid: i32) {
        if self.injecao_rx.is_some() {
            return;
        }
        let Some(inj) = injetor_path() else {
            self.ult_msg = "bitu-inject not found next to the binary".to_string();
            return;
        };
        let Some(lib) = self.lib_path.clone().filter(|p| p.is_file()) else {
            self.ult_msg = "libbitu_layer.so missing".to_string();
            return;
        };
        // seleciona já: o accept do servidor manda o alvo assim que a layer conectar
        if let Ok(mut g) = self.compart.alvo_pid.lock() {
            *g = pid;
        }
        self.enviar_alvo();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let res = match std::process::Command::new(&inj)
                .arg(pid.to_string())
                .arg(&lib)
                .output()
            {
                Err(e) => Err(e.to_string()),
                Ok(o) if o.status.success() => {
                    let out = String::from_utf8_lossy(&o.stdout);
                    let mask = out
                        .trim()
                        .strip_prefix("ok ")
                        .and_then(|n| n.parse::<u32>().ok())
                        .unwrap_or(0);
                    Ok(mask)
                }
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    Err(if err.is_empty() { "injetor falhou".to_string() } else { err })
                }
            };
            let _ = tx.send((pid, res));
        });
        self.injecao_rx = Some(rx);
        self.ult_msg = format!("attaching to {pid}…");
    }

    /// Coleta o resultado de uma injeção em andamento.
    fn poll_injecao(&mut self) {
        let Some(rx) = &self.injecao_rx else { return };
        let recebido = match rx.try_recv() {
            Ok(v) => Some(v),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.injecao_rx = None;
                None
            }
        };
        if let Some((pid, res)) = recebido {
            self.injecao_rx = None;
            match res {
                Ok(mask) if mask != 0 => self.ult_msg = format!("attached to {pid}"),
                Ok(_) => self.ult_msg = format!("pid {pid}: no gl/vk present"),
                Err(e) => {
                    self.ult_msg = e;
                    let sel = self.compart.alvo_pid.lock().map(|g| *g).unwrap_or(0);
                    if sel == pid {
                        if let Ok(mut g) = self.compart.alvo_pid.lock() {
                            *g = 0;
                        }
                        self.enviar_alvo();
                    }
                }
            }
        }
    }

}

impl eframe::App for Bitu {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(33));
        // uma vez por sessão: confirma na prática o tamanho fixo pedido.
        // (No Wayland o compositor não informa a posição, então inner_rect
        // fica None; screen_rect é a área de pintura efetiva.)
        if !self.logou_tamanho {
            self.logou_tamanho = true;
            let r = ctx.content_rect();
            if (r.width() - JANELA_L).abs() > 1.0 || (r.height() - JANELA_A).abs() > 1.0 {
                eprintln!(
                    "bitu: área {:.0}x{:.0} (esperado {JANELA_L}x{JANELA_A})",
                    r.width(),
                    r.height()
                );
            } else {
                eprintln!("bitu: janela fixa {:.0}x{:.0} ok", r.width(), r.height());
            }
        }
        // lista de processos + resultado de injeção a cada 500 ms
        if std::time::Instant::now() >= self.prox_refresh {
            self.prox_refresh = std::time::Instant::now() + Duration::from_millis(500);
            self.atualizar_lista();
            self.poll_injecao();
        }
        desenhar(ctx, self);
    }
}

fn desenhar(ctx: &egui::Context, b: &mut Bitu) {
    let sel_pid = b.compart.alvo_pid.lock().map(|g| *g).unwrap_or(0);

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(egui::Color32::BLACK)
                .inner_margin(egui::Margin::same(6)),
        )
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                cartao(ui, 168.0, 66.0, |ui| cartao_titulo(ui, b));
                cartao(ui, 272.0, 66.0, |ui| cartao_telemetria(ui, &b.compart, sel_pid));
                cartao(ui, 168.0, 66.0, |ui| cartao_alvo(ui, b, sel_pid));
            });
            ui.add_space(4.0);
            grafico(ui, &b.compart.samples_us, ALVO_US, sel_pid);
        });

    if b.popup_aberto {
        popup_pids(ctx, b, sel_pid);
    }
}

/// Cartão cinza de tamanho fixo (o conteúdo não estica o cartão).
/// O conteúdo é sempre vertical — a linha dos cartões é horizontal, mas
/// cada cartão não pode herdar esse layout.
fn cartao(ui: &mut egui::Ui, largura: f32, altura: f32, conteudo: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(COR_CARTAO)
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_min_size(egui::vec2(largura - 16.0, altura - 16.0));
            ui.set_max_size(egui::vec2(largura - 16.0, altura - 16.0));
            ui.vertical(|ui| {
                conteudo(ui);
            });
        });
}

fn rotulo(txt: &str) -> egui::RichText {
    egui::RichText::new(txt).size(11.0).color(COR_TEXTO_SUAVE)
}

fn valor(txt: String) -> egui::RichText {
    egui::RichText::new(txt).monospace().size(13.0)
}

fn cartao_titulo(ui: &mut egui::Ui, b: &mut Bitu) {
    let sel = b.compart.alvo_pid.lock().map(|g| *g).unwrap_or(0);
    let ativo = sel != 0 && b.lista.iter().any(|a| a.pid == sel && a.conectado);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(format!("Bitu v{}", env!("CARGO_PKG_VERSION")))
                    .size(15.0)
                    .strong(),
            );
            ui.add_space(1.0);
            ui.label(rotulo("frame limiter"));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if botao_jogo(ui, ativo).on_hover_text("attach to process").clicked() {
                b.popup_aberto = !b.popup_aberto;
            }
        });
    });
}

/// Ícone de controle: clique abre o popup de PIDs. Verde quando há app
/// selecionado.
fn botao_jogo(ui: &mut egui::Ui, ativo: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(26.0, 22.0), egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(4),
            egui::Color32::from_rgb(62, 64, 70),
        );
    }
    let p = ui.painter_at(rect);
    let cor = if ativo {
        COR_VERDE
    } else {
        egui::Color32::from_rgb(205, 207, 212)
    };
    let corpo = egui::Rect::from_center_size(rect.center(), egui::vec2(20.0, 12.0));
    p.rect_filled(corpo, egui::CornerRadius::same(5), cor);
    let escuro = COR_CARTAO;
    let cx = corpo.left() + 6.0;
    let cy = corpo.center().y;
    p.line_segment(
        [egui::pos2(cx - 2.2, cy), egui::pos2(cx + 2.2, cy)],
        egui::Stroke::new(1.6_f32, escuro),
    );
    p.line_segment(
        [egui::pos2(cx, cy - 2.2), egui::pos2(cx, cy + 2.2)],
        egui::Stroke::new(1.6_f32, escuro),
    );
    p.circle_filled(egui::pos2(corpo.right() - 6.5, cy - 2.0), 1.3, escuro);
    p.circle_filled(egui::pos2(corpo.right() - 4.0, cy + 1.5), 1.3, escuro);
    resp
}

fn cartao_telemetria(ui: &mut egui::Ui, compart: &Compartilhado, sel_pid: i32) {
    let st = estatisticas(&compart.samples_us, ALVO_US, sel_pid);
    let (fps, avg_fps, ft, avg_ft) = match st {
        Some(s) => (
            format!("{:5.1}", s.fps),
            format!("{:5.1}", s.fps_medio),
            format!("{:5.2} ms", s.ft_ms),
            format!("{:5.2} ms", s.ft_medio_ms),
        ),
        None => (
            "—".to_string(),
            "—".to_string(),
            "—".to_string(),
            "—".to_string(),
        ),
    };
    egui::Grid::new("telemetria")
        .num_columns(4)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label(rotulo("fps"));
            ui.label(valor(fps));
            ui.label(rotulo("avg fps"));
            ui.label(valor(avg_fps));
            ui.end_row();

            ui.label(rotulo("FT"));
            ui.label(valor(ft));
            ui.label(rotulo("avg FT"));
            ui.label(valor(avg_ft));
            ui.end_row();
        });
}

fn cartao_alvo(ui: &mut egui::Ui, b: &Bitu, sel_pid: i32) {
    ui.label(rotulo("frame time by app"));
    ui.label(
        egui::RichText::new(format!(
            "{:.2} ms ({:.0} fps)",
            ALVO_US as f64 / 1000.0,
            1e6 / ALVO_US as f64
        ))
        .size(13.0)
        .strong(),
    );
    ui.add_space(2.0);
    let app = b.lista.iter().find(|a| a.pid == sel_pid);
    let txt = match app {
        Some(a) => format!("{} · {}", a.pid, trunc(&a.nome, 16)),
        None => "—".to_string(),
    };
    ui.label(egui::RichText::new(txt).size(9.5).color(COR_TEXTO_SUAVE));
}

fn popup_pids(ctx: &egui::Context, b: &mut Bitu, sel_pid: i32) {
    let conectado = b
        .lista
        .iter()
        .find(|a| a.pid == sel_pid)
        .map(|a| a.conectado)
        .unwrap_or(false);
    let injetando = b.injecao_rx.is_some();
    let mut aberto = true;
    egui::Window::new("attach to process")
        .open(&mut aberto)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .default_width(430.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(rotulo("filter"));
                ui.add(egui::TextEdit::singleline(&mut b.busca).desired_width(140.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if injetando {
                        let _ = ui.small_button("attaching…");
                    } else {
                        if sel_pid != 0 && !conectado && ui.small_button("attach").clicked() {
                            b.anexar(sel_pid);
                        }
                        if sel_pid != 0 && ui.small_button("detach").clicked() {
                            b.definir_sel(0);
                        }
                    }
                });
            });
            egui::ScrollArea::vertical()
                .max_height(64.0)
                .id_salt("apps")
                .show(ui, |ui| {
                    let busca = b.busca.trim().to_lowercase();
                    let mut clicado: Option<i32> = None;
                    for app in &b.lista {
                        if !busca.is_empty()
                            && !app.nome.to_lowercase().contains(&busca)
                            && !app.cmd.to_lowercase().contains(&busca)
                        {
                            continue;
                        }
                        let marcado = app.pid == sel_pid;
                        let marca = if app.conectado { " ✓" } else { "" };
                        let linha = format!("{} · {}{}", app.pid, trunc(&app.nome, 30), marca);
                        if ui
                            .selectable_label(marcado, linha)
                            .on_hover_text(&app.cmd)
                            .clicked()
                        {
                            clicado = Some(if marcado { 0 } else { app.pid });
                        }
                    }
                    if let Some(pid) = clicado {
                        b.definir_sel(pid);
                    }
                });
            if !b.ult_msg.is_empty() {
                ui.label(
                    egui::RichText::new(trunc(&b.ult_msg, 72))
                        .size(10.0)
                        .color(COR_TEXTO_SUAVE),
                );
            }
        });
    if !aberto {
        b.popup_aberto = false;
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let skip = s.chars().count() - n;
        let tail: String = s.chars().skip(skip).collect();
        format!("…{tail}")
    }
}

/// Valores do cartão de telemetria (PID selecionado, janela do gráfico).
#[derive(Clone, Copy)]
struct Stats {
    fps: f64,
    fps_medio: f64,
    ft_ms: f64,
    ft_medio_ms: f64,
}

fn estatisticas(
    samples: &Mutex<VecDeque<(i32, u32, u32, u32, u32)>>,
    alvo_us: u32,
    sel: i32,
) -> Option<Stats> {
    let dados: Vec<f64> = samples
        .lock()
        .ok()?
        .iter()
        .rev()
        .filter(|&&(p, _, _, _, a)| p == sel && do_regime(a, alvo_us))
        .take(JANELA_STATS)
        .map(|&(_, u, _, _, _)| u as f64)
        .collect();
    if dados.is_empty() {
        return None;
    }
    let ft = dados[0].max(1.0);
    let media = dados.iter().sum::<f64>() / dados.len() as f64;
    Some(Stats {
        fps: 1e6 / ft,
        ft_ms: ft / 1000.0,
        fps_medio: if media > 0.0 { 1e6 / media } else { 0.0 },
        ft_medio_ms: media / 1000.0,
    })
}

/// Cor por causa e gravidade:
///   verde   = dentro de ±1 ms do alvo;
///   amarelo = desvio de 1–3 ms sem causa definida;
///   laranja = atraso do jogo (entrada depois do deadline) ou desvio >3 ms;
///   vermelho = atraso do pacer (wake ≥1 ms), que é o que o limitador
///              precisa eliminar.
fn nivel(ms: f32, atraso_us: u32, acordou_us: u32, alvo_ft_ms: f32) -> u8 {
    if alvo_ft_ms <= 0.0 {
        return 0; // sem limite: não há alvo para violar
    }
    let desvio = (ms - alvo_ft_ms).abs();
    let jogo_us = atraso_us.saturating_sub(acordou_us);
    if acordou_us >= 1000 {
        3
    } else if jogo_us >= 1000 || desvio > 3.0 {
        2
    } else if desvio > 1.0 || atraso_us >= 1000 {
        1
    } else {
        0
    }
}

fn grafico(
    ui: &mut egui::Ui,
    samples: &Mutex<VecDeque<(i32, u32, u32, u32, u32)>>,
    alvo_us: u32,
    sel: i32,
) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ui.available_height()),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::same(6), COR_CARTAO);

    let alvo_ft_ms = alvo_us as f32 / 1000.0;
    let dados: Vec<(f32, u32, u32)> = samples
        .lock()
        .map(|q| {
            let mut v: Vec<(f32, u32, u32)> = q
                .iter()
                .rev()
                .filter(|&&(p, _, _, _, a)| p == sel && do_regime(a, alvo_us))
                .take(PONTOS_GRAFICO)
                .map(|&(_, u, atraso, acordou, _)| (u as f32 / 1000.0, atraso, acordou))
                .collect();
            v.reverse(); // cronológico da esquerda para a direita
            v
        })
        .unwrap_or_default();

    // Janela em torno do alvo: ±(18% do alvo, entre 2 e 8 ms). Assim uma
    // variação de 0,5 ms fica visível; picos maiores são marcados no topo.
    let (min_ms, max_ms) = if alvo_ft_ms > 0.0 {
        let span = (alvo_ft_ms * 0.18).clamp(2.0, 8.0);
        ((alvo_ft_ms - span).max(0.0), alvo_ft_ms + span)
    } else {
        let m = dados.iter().map(|d| d.0).fold(4.0_f32, f32::max).max(4.0);
        (0.0, m)
    };
    let h = (rect.height() - 18.0).max(10.0);
    let y = |ms: f32| {
        let t = ((ms - min_ms) / (max_ms - min_ms).max(0.001)).clamp(0.0, 1.0);
        rect.top() + 9.0 + (1.0 - t) * h
    };
    // Janela fixa: a linha cresce da direita para a esquerda sem reescalar
    // todos os pontos a cada amostra nova (isso fazia a linha parecer mexer).
    let x_step = rect.width() / (PONTOS_GRAFICO - 1) as f32;
    let x0 = rect.left() + (PONTOS_GRAFICO.saturating_sub(dados.len())) as f32 * x_step;

    // Só a linha do alvo: sem rótulos de texto; a informação está nas cores.
    if alvo_ft_ms > 0.0 {
        let yr = y(alvo_ft_ms);
        painter.line_segment(
            [egui::pos2(rect.left() + 4.0, yr), egui::pos2(rect.right() - 4.0, yr)],
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(90, 150, 100)),
        );
    }

    let cores = [COR_VERDE, COR_AMARELO, COR_LARANJA, COR_VERMELHO];
    if dados.len() > 1 {
        let pts: Vec<egui::Pos2> = dados
            .iter()
            .enumerate()
            .map(|(i, &(ms, _, _))| egui::pos2(x0 + i as f32 * x_step, y(ms)))
            .collect();
        let niveis: Vec<u8> = dados
            .iter()
            .map(|&(ms, atraso, acordou)| nivel(ms, atraso, acordou, alvo_ft_ms))
            .collect();
        // Polilinha contínua por trecho de cor: um único Shape::line por
        // trecho. Desenhar 1 segmento por amostra criava emenda de antialias
        // a cada ~1 px — com o FT em cima da linha do alvo, o alvo aparecia
        // entre as emendas e a linha parecia tracejada.
        let mut inicio = 0usize;
        let mut n_atual = niveis[0].max(niveis[1]);
        for i in 1..pts.len() - 1 {
            let n = niveis[i].max(niveis[i + 1]);
            if n != n_atual {
                painter.add(egui::Shape::line(
                    pts[inicio..=i].to_vec(),
                    egui::Stroke::new(1.3_f32, cores[n_atual as usize]),
                ));
                inicio = i;
                n_atual = n;
            }
        }
        painter.add(egui::Shape::line(
            pts[inicio..].to_vec(),
            egui::Stroke::new(1.3_f32, cores[n_atual as usize]),
        ));
        // Picos fora da janela: risco no topo com a cor da causa.
        for (i, &(ms, atraso, acordou)) in dados.iter().enumerate() {
            if ms > max_ms {
                let jogo_us = atraso.saturating_sub(acordou);
                let cor = if acordou >= 1000 {
                    COR_VERMELHO
                } else if jogo_us > 0 || alvo_us > 0 {
                    COR_LARANJA
                } else {
                    COR_AMARELO
                };
                let x = x0 + i as f32 * x_step;
                painter.line_segment(
                    [
                        egui::pos2(x, rect.top() + 9.0),
                        egui::pos2(x, rect.top() + 16.0),
                    ],
                    egui::Stroke::new(1.5_f32, cor),
                );
            }
        }
    }
}

fn main() -> Result<(), eframe::Error> {
    let compart = Arc::new(Compartilhado::new());
    if let Some(sp) = sock_path() {
        if let Err(e) = iniciar_servidor(sp, compart.clone()) {
            eprintln!("bitu: falha no servidor IPC: {e}");
        }
    }

    // alvo fixo em efeito desde o boot (30 fps / 33,33 ms)
    if let Ok(mut g) = compart.alvo_us.lock() {
        *g = ALVO_US;
    }

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([JANELA_L, JANELA_A])
            .with_min_inner_size([JANELA_L, JANELA_A])
            .with_max_inner_size([JANELA_L, JANELA_A])
            .with_resizable(false)
            .with_maximize_button(false)
            .with_title("Bitu"),
        ..Default::default()
    };

    eframe::run_native(
        "Bitu",
        opts,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(visual_escuro());
            Ok(Box::new(Bitu::novo(compart)) as Box<dyn eframe::App>)
        }),
    )
}

/// Fundo preto, cartões cinza; janela do popup num cinza um pouco mais claro.
fn visual_escuro() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = egui::Color32::BLACK;
    v.window_fill = egui::Color32::from_rgb(28, 28, 32);
    v.window_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(70, 72, 78));
    v.extreme_bg_color = egui::Color32::from_rgb(16, 16, 18);
    v
}
