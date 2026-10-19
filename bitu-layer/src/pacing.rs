//! Núcleo de pacing de frames do Bitu.
//!
//! Objetivo: travar o *frame time* num alvo fixo ao estilo console.
//! A regra é de intervalo mínimo: o próximo present só é liberado quando
//! `última_liberação + alvo` chega. Se o jogo atrasar, o frame atrasado é
//! liberado na hora e o próximo recebe um período cheio — nunca encurtamos
//! os frames seguintes para "recuperar" a média. Um frame perdido é um
//! frame perdido, como num console.

use std::ffi::CString;
use std::sync::{Mutex, OnceLock};

/// Tempo monotônico em nanosegundos.
#[inline]
fn now_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

fn env_nonempty(name: &str) -> Option<CString> {
    std::env::var(name).ok().and_then(|p| {
        let p = p.trim().to_string();
        if p.is_empty() {
            None
        } else {
            CString::new(p).ok()
        }
    })
}

/// Caminho padrão do socket da GUI (modo "anexar", quando BITU_SOCK não está no ambiente).
/// Fixo para que a layer — carregada implicitamente em qualquer app Vulkan pela
/// manifest em ~/.local/share/vulkan/implicit_layer.d — encontre a GUI sozinha.
fn sock_padrao() -> Option<Vec<u8>> {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    Some(format!("{base}/bitu/bitu.sock").into_bytes())
}

/// Socket ABSTRATO do Linux (`\0bitu-<uid>`): não é arquivo, então atravessa
/// sandboxes (Flatpak/pressure-vessel) que filtram o /run/user — desde que
/// compartilhem o network namespace (o caso comum).
fn sock_abstrato() -> Vec<u8> {
    let mut v = vec![0u8];
    v.extend_from_slice(format!("bitu-{}", unsafe { libc::getuid() }).as_bytes());
    v
}

/// Conecta num endereço unix em bytes: nome abstrato (1º byte 0) ou caminho
/// de arquivo. Devolve o fd pronto (não-bloqueante) ou None.
unsafe fn conectar_unix(ala: &[u8], debug: bool) -> Option<i32> {
    let fd = libc::socket(
        libc::AF_UNIX,
        libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
        0,
    );
    if fd < 0 {
        return None;
    }
    let mut sa: libc::sockaddr_un = std::mem::zeroed();
    sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let len: libc::socklen_t = if ala.first() == Some(&0) {
        if ala.len() > sa.sun_path.len() {
            libc::close(fd);
            return None;
        }
        for (i, &b) in ala.iter().enumerate() {
            sa.sun_path[i] = b as libc::c_char;
        }
        // offsetof(sockaddr_un, sun_path) = 2; abstrato inclui o NUL inicial
        (std::mem::size_of::<libc::sa_family_t>() + ala.len()) as libc::socklen_t
    } else {
        if ala.len() + 1 > sa.sun_path.len() {
            libc::close(fd);
            return None;
        }
        for (i, &b) in ala.iter().enumerate() {
            sa.sun_path[i] = b as libc::c_char;
        }
        std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t
    };
    if libc::connect(fd, &sa as *const _ as *const libc::sockaddr, len) != 0 {
        if debug {
            eprintln!("[bitu] ipc connect falhou: {}", std::io::Error::last_os_error());
        }
        libc::close(fd);
        return None;
    }
    Some(fd)
}

/// Cliente IPC (socket unix seqpacket) com a GUI do Bitu.
/// Envia o frame time de cada frame e recebe atualizações do alvo ao vivo.
struct IpcClient {
    fd: i32, // -1 = desconectado
    alvos: Vec<Vec<u8>>, // abstrato e/ou caminho de arquivo; BITU_SOCK força um só
    retry_at_ns: i64,
    frames: u64,
    debug_log: bool,
}

impl IpcClient {
    fn new() -> Self {
        let alvos = match env_nonempty("BITU_SOCK") {
            Some(p) => vec![p.into_bytes()],
            None => {
                // abstrato primeiro: é o único que atravessa sandbox; o arquivo
                // fica de fallback para redes/namespaces sem o abstrato.
                let mut v = vec![sock_abstrato()];
                if let Some(p) = sock_padrao() {
                    v.push(p);
                }
                v
            }
        };
        IpcClient {
            fd: -1,
            alvos,
            retry_at_ns: 0,
            frames: 0,
            debug_log: std::env::var("BITU_DEBUG").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false),
        }
    }

    fn try_connect(&mut self, now: i64) {
        if self.fd >= 0 {
            return;
        }
        if now < self.retry_at_ns {
            return; // espera para tentar de novo
        }
        self.retry_at_ns = now + 2_000_000_000; // tentar a cada 2 s
        for ala in &self.alvos {
            if let Some(fd) = unsafe { conectar_unix(ala, self.debug_log) } {
                if self.debug_log {
                    eprintln!("[bitu] ipc conectado");
                }
                self.fd = fd;
                return;
            }
        }
    }

    /// Amostra = ft entre liberações reais (u32 µs), contador, atraso em
    /// relação ao deadline (u32 µs), atraso de wake do pacer (u32 µs; 0 =
    /// o atraso, se houver, foi do jogo) e alvo usado no frame (u32 µs;
    /// 0 = sem limite). O alvo permite à GUI não misturar regimes: amostra
    /// de antes do limite ou de outro preset não entra no gráfico atual.
    fn send_sample(&mut self, ft_ns: i64, late_ns: i64, acordou_ns: i64, alvo_ns: i64) {
        if self.fd < 0 {
            return;
        }
        self.frames = self.frames.wrapping_add(1);
        let ft_us = (ft_ns / 1000).clamp(0, u32::MAX as i64) as u32;
        let late_us = (late_ns / 1000).clamp(0, u32::MAX as i64) as u32;
        let acordou_us = (acordou_ns / 1000).clamp(0, u32::MAX as i64) as u32;
        let alvo_us = (alvo_ns / 1000).clamp(0, u32::MAX as i64) as u32;
        let mut buf = [0u8; 20];
        buf[..4].copy_from_slice(&ft_us.to_le_bytes());
        buf[4..8].copy_from_slice(&(self.frames as u32).to_le_bytes());
        buf[8..12].copy_from_slice(&late_us.to_le_bytes());
        buf[12..16].copy_from_slice(&acordou_us.to_le_bytes());
        buf[16..].copy_from_slice(&alvo_us.to_le_bytes());
        unsafe {
            libc::send(self.fd, buf.as_ptr() as *const libc::c_void, 20, libc::MSG_NOSIGNAL);
        }
    }

    fn poll_target(&mut self) -> PollAlvo {
        if self.fd < 0 {
            return PollAlvo::Nada;
        }
        let mut buf = [0u8; 8];
        let n = unsafe {
            libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, 8, libc::MSG_DONTWAIT)
        };
        if n == 8 {
            return PollAlvo::Alvo(u32::from_le_bytes(buf[..4].try_into().unwrap()));
        }
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EAGAIN) {
                return PollAlvo::Nada;
            }
        }
        if n == 0 || errno_ipc_morta() {
            // GUI fechou a conexão
            unsafe { libc::close(self.fd) };
            self.fd = -1;
            self.retry_at_ns = now_ns() + 2_000_000_000;
            return PollAlvo::Caiu;
        }
        PollAlvo::Nada
    }
}

fn errno_ipc_morta() -> bool {
    let e = std::io::Error::last_os_error();
    matches!(
        e.raw_os_error(),
        Some(libc::EPIPE) | Some(libc::ECONNRESET) | Some(libc::ENOTCONN)
    )
}

/// Resposta da leitura do socket: nada novo, alvo novo, ou GUI caiu.
enum PollAlvo {
    Nada,
    Alvo(u32),
    Caiu,
}

/// Margem de busy-wait: começa em 1 ms e cresce se o `clock_nanosleep`
/// acordar depois do deadline. O teto não é para "compensar" o jogo; é só
/// para cobrir a latência de wake da máquina (C-state/scheduler).
const SPIN_MIN_NS: i64 = 1_000_000;
const SPIN_MAX_NS: i64 = 5_000_000;
const SPIN_FOLGA_NS: i64 = 300_000;
const SPIN_DECAI_FRAMES: u64 = 64;
const SPIN_DECAI_NS: i64 = 1_000;

/// Diagnóstico interno: separa atraso do jogo (entrada) do atraso do pacer
/// (wake do nanosleep) e de preempção durante o spin.
#[derive(Default)]
struct Diag {
    frames: u64,
    entrada_tarde: u64,
    entrada_tarde_max_ns: i64,
    acordou_tarde: u64,
    acordou_tarde_max_ns: i64,
    spin_tarde: u64,
    spin_tarde_max_ns: i64,
    pico_acordo_ns: i64, // latência de wake estimada (late + spin usado)
}

/// Agendador puro (sem relógio/sleep) para poder testar a regra de cadência.
#[derive(Clone, Copy)]
struct Agenda {
    ultimo_ns: i64, // última liberação real; 0 = ainda não houve
}

impl Agenda {
    fn reiniciar(&mut self) {
        self.ultimo_ns = 0;
    }

    fn deadline(&self, target_ns: i64) -> i64 {
        self.ultimo_ns.saturating_add(target_ns)
    }

    /// Registra uma liberação e devolve (ft, atraso em relação ao deadline).
    fn liberar(&mut self, release_ns: i64, deadline_ns: i64) -> (i64, i64) {
        let ft = (release_ns - self.ultimo_ns).max(0);
        let late = (release_ns - deadline_ns).max(0);
        self.ultimo_ns = release_ns;
        (ft, late)
    }
}

/// Pacer compartilhado por API (Vulkan/GL) do processo inteiro.
pub struct Pacer {
    target_ns: i64, // período alvo (0 = sem limite)
    /// alvo declarado por env (BITU_FPS); volta a valer quando a GUI desconecta
    env_target_ns: i64,
    agenda: Agenda,
    spin_ns: i64, // resto fino por busy-wait
    ipc: IpcClient,
    debug: bool,
    diag: Diag,
}

impl Pacer {
    fn new() -> Self {
        let debug = std::env::var("BITU_DEBUG")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        // Alvo via env (modo "lançar" da GUI; no modo anexar fica 0 = inerte até a GUI selecionar).
        let mut env_target_ns = 0i64;
        if let Some(fps) = std::env::var("BITU_FPS")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|f| *f > 0.0)
        {
            env_target_ns = (1e9 / fps) as i64;
        }

        if debug {
            eprintln!("[bitu] layer ativa: alvo env = {} ns", env_target_ns);
        }

        Pacer {
            target_ns: env_target_ns,
            env_target_ns,
            agenda: Agenda { ultimo_ns: 0 },
            spin_ns: SPIN_MIN_NS, // busy-wait inicial; cresce se o wake atrasar
            ipc: IpcClient::new(),
            debug,
            diag: Diag::default(),
        }
    }

    /// Chamado no início de cada present. Mede a cadência de liberação e
    /// espera até `última_liberação + alvo` quando o jogo chegou cedo.
    pub fn frame_enter(&mut self) {
        let now = now_ns();

        // alvo vivo (GUI seleciona/desativa por PID; queda da GUI volta ao env)
        match self.ipc.poll_target() {
            PollAlvo::Alvo(us) => {
                let new_target = (us as i64) * 1000;
                if new_target != self.target_ns {
                    if self.debug {
                        eprintln!("[bitu] alvo alterado: {} us", us);
                    }
                    self.target_ns = new_target;
                    // O frame atual volta a ser imediato; o próximo já usa o
                    // novo período, sem carregar atraso do alvo anterior.
                    self.agenda.reiniciar();
                }
            }
            PollAlvo::Caiu => {
                if self.target_ns != self.env_target_ns {
                    if self.debug {
                        eprintln!("[bitu] GUI saiu — alvo volta ao valor do env");
                    }
                    self.target_ns = self.env_target_ns;
                    self.agenda.reiniciar();
                }
            }
            PollAlvo::Nada => {}
        }
        if self.ipc.fd < 0 {
            // conecta sempre (até conectado): é assim que a GUI lista os apps Vulkan
            self.ipc.try_connect(now);
        }

        // Sem limite: ainda manda telemetria da cadência de entrada, mas não
        // segura nenhum frame.
        if self.target_ns == 0 {
            if self.agenda.ultimo_ns != 0 {
                let ft = (now - self.agenda.ultimo_ns).max(0);
                if self.ipc.fd >= 0 {
                    self.ipc.send_sample(ft, 0, 0, 0);
                }
            }
            self.agenda.ultimo_ns = now;
            return;
        }

        // Primeiro frame com limite (ou logo após troca de alvo): apresenta
        // imediatamente e usa esta liberação como base para o próximo slot.
        if self.agenda.ultimo_ns == 0 {
            self.agenda.ultimo_ns = now;
            if self.debug {
                eprintln!("[bitu] pacing iniciado, período = {} ns", self.target_ns);
            }
            return;
        }

        let deadline = self.agenda.deadline(self.target_ns);
        self.diag.frames = self.diag.frames.wrapping_add(1);
        if now >= deadline {
            let atraso = now - deadline;
            self.diag.entrada_tarde = self.diag.entrada_tarde.wrapping_add(1);
            self.diag.entrada_tarde_max_ns = self.diag.entrada_tarde_max_ns.max(atraso);
        }

        let mut dormiu = false;
        let mut acordou_em = now;
        let mut spin_usado = 0i64;
        if now < deadline {
            let (d, t, s) = self.sleep_until(deadline);
            dormiu = d;
            acordou_em = t;
            spin_usado = s;
        }

        // `now_ns()` depois do sleep é o instante real de liberação. Se o jogo
        // chegou atrasado, ele sai agora; o próximo deadline parte daqui.
        let release = now_ns();
        let (ft, late) = self.agenda.liberar(release, deadline);

        // Separa as duas causas de atraso: entrada (jogo) e wake (pacer).
        let acordou_late = if dormiu { (acordou_em - deadline).max(0) } else { 0 };
        let spin_late = if acordou_em < deadline { (release - deadline).max(0) } else { 0 };
        if acordou_late > 0 {
            self.diag.acordou_tarde_max_ns = self.diag.acordou_tarde_max_ns.max(acordou_late);
            if acordou_late > 100_000 {
                self.diag.acordou_tarde = self.diag.acordou_tarde.wrapping_add(1);
            }
        }
        if spin_late > 0 {
            self.diag.spin_tarde_max_ns = self.diag.spin_tarde_max_ns.max(spin_late);
            if spin_late > 100_000 {
                self.diag.spin_tarde = self.diag.spin_tarde.wrapping_add(1);
            }
        }
        self.ajustar_spin(dormiu, spin_usado, acordou_late);

        if self.ipc.fd >= 0 {
            self.ipc.send_sample(ft, late, acordou_late, self.target_ns);
        }
    }

    /// Dorme com precisão: clock_nanosleep absoluto até `deadline - spin` e
    /// busy-wait no último trecho. O wake pode atrasar; quando atrasa, ele é
    /// medido e a margem de spin cresce para as próximas liberações.
    fn sleep_until(&mut self, deadline: i64) -> (bool, i64, i64) {
        let spin = self.spin_efetivo();
        let despertar = deadline.saturating_sub(spin);
        let mut dormiu = false;
        let mut acordou_em = now_ns();

        if acordou_em < despertar {
            dormiu = true;
            let ts = libc::timespec {
                tv_sec: (despertar / 1_000_000_000) as libc::time_t,
                tv_nsec: (despertar % 1_000_000_000) as libc::c_long,
            };
            loop {
                let rc = unsafe {
                    libc::clock_nanosleep(
                        libc::CLOCK_MONOTONIC,
                        libc::TIMER_ABSTIME,
                        &ts,
                        std::ptr::null_mut(),
                    )
                };
                if rc != libc::EINTR {
                    break;
                }
            }
            acordou_em = now_ns();
        }

        // busy-wait final: precisão de microssegundos e nenhum overshoot de
        // nanosleep. O tempo acordado é contado no diagnóstico.
        loop {
            if now_ns() >= deadline {
                return (dormiu, acordou_em, spin);
            }
            std::hint::spin_loop();
        }
    }

    /// Não ocupar mais que metade do período (a GUI permite alvos de 1 ms).
    fn spin_efetivo(&self) -> i64 {
        self.spin_ns
            .min((self.target_ns / 2).max(100_000))
            .max(0)
    }

    /// Se o nanosleep acordou depois do deadline, a latência real foi
    /// `atraso + spin`; guarda o pior caso e aumenta a margem. Sem atraso, a
    /// margem cai muito devagar (1 µs a cada 64 frames), para não oscilar.
    fn ajustar_spin(&mut self, dormiu: bool, spin_usado: i64, acordou_late: i64) {
        if dormiu && acordou_late > 0 {
            let latencia = acordou_late.saturating_add(spin_usado);
            self.diag.pico_acordo_ns = self.diag.pico_acordo_ns.max(latencia);
            self.spin_ns = self
                .diag
                .pico_acordo_ns
                .saturating_add(SPIN_FOLGA_NS)
                .clamp(SPIN_MIN_NS, SPIN_MAX_NS);
        } else if dormiu
            && self.diag.frames % SPIN_DECAI_FRAMES == 0
            && self.diag.pico_acordo_ns > 0
        {
            self.diag.pico_acordo_ns = (self.diag.pico_acordo_ns - SPIN_DECAI_NS).max(0);
            self.spin_ns = self
                .diag
                .pico_acordo_ns
                .saturating_add(SPIN_FOLGA_NS)
                .clamp(SPIN_MIN_NS, SPIN_MAX_NS);
        }
    }
}

static PACER: OnceLock<Mutex<Pacer>> = OnceLock::new();

fn pacer() -> &'static Mutex<Pacer> {
    PACER.get_or_init(|| Mutex::new(Pacer::new()))
}

/// Força a inicialização do Pacer FORA do handler de sinal. O injetor chama
/// isto ao instalar os hooks: evita que a primeira alocação (env vars,
/// CString, OnceLock) aconteça dentro do handler — malloc não é
/// async-signal-safe.
pub fn pre_aquecer() {
    let _ = pacer();
}

/// A layer já foi ativada pelo caminho normal (manifest implícito/preload)?
/// O injetor usa isto para NÃO instalar hooks int3 em cima dos de interposição
/// (contaria cada present 2× e dobraria o período do pacing).
pub fn ja_iniciado() -> bool {
    PACER.get().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadencia_estavel() {
        // alvo de 3 ms; 80 frames; confere cadência média (o primeiro frame
        // é o "imediato" após ativar e não entra na média)
        let mut p = Pacer {
            target_ns: 3_000_000,
            env_target_ns: 0,
            agenda: Agenda { ultimo_ns: 0 },
            spin_ns: 200_000,
            ipc: IpcClient { fd: -1, alvos: Vec::new(), retry_at_ns: i64::MAX, frames: 0, debug_log: false },
            debug: false,
            diag: Diag::default(),
        };
        let mut periods = Vec::new();
        for i in 0..80 {
            let before = now_ns();
            p.frame_enter();
            let after = now_ns();
            if i > 0 {
                periods.push(after - before);
            }
        }
        let media: f64 = periods.iter().sum::<i64>() as f64 / periods.len() as f64;
        let err = (media - 3_000_000.0).abs() / 3_000_000.0;
        assert!(err < 0.15, "cadência média {:.1} µs (erro {:.1}%)", media / 1000.0, err * 100.0);
    }

    #[test]
    fn atraso_nao_encurta_o_proximo_frame() {
        // Regra de console: um frame atrasado não faz os seguintes rodarem
        // mais rápido para "recuperar" a média.
        let alvo = 33_333_000;
        let mut a = Agenda { ultimo_ns: 1_000_000 };
        let d1 = a.deadline(alvo);
        assert_eq!(d1, 34_333_000);

        let atraso = 20_000_000;
        let release = d1 + atraso;
        let (ft, late) = a.liberar(release, d1);
        assert_eq!(late, atraso);
        assert_eq!(ft, release - 1_000_000);

        // O próximo slot parte da liberação real, não do slot antigo.
        let d2 = a.deadline(alvo);
        assert_eq!(d2, release + alvo);
        let (ft2, late2) = a.liberar(d2, d2);
        assert_eq!(ft2, alvo, "o frame seguinte deve ter o período cheio");
        assert_eq!(late2, 0);
    }

    fn pacer_teste(target_ns: i64, spin_ns: i64) -> Pacer {
        Pacer {
            target_ns,
            env_target_ns: 0,
            agenda: Agenda { ultimo_ns: 0 },
            spin_ns,
            ipc: IpcClient {
                fd: -1,
                alvos: Vec::new(),
                retry_at_ns: i64::MAX,
                frames: 0,
                debug_log: false,
            },
            debug: false,
            diag: Diag::default(),
        }
    }

    #[test]
    fn spin_cobre_wake_tardio() {
        // Wake de 2,609 ms com spin de 1 ms: o próximo spin precisa ser
        // 2,609 ms + folga. O atraso do jogo (entrada) não mexe no spin.
        let mut p = pacer_teste(33_333_000, 1_000_000);
        p.ajustar_spin(true, 1_000_000, 1_609_000);
        assert_eq!(p.diag.pico_acordo_ns, 2_609_000);
        assert_eq!(p.spin_ns, 2_909_000);

        let antes = p.spin_ns;
        p.ajustar_spin(false, 0, 0); // entrada atrasada: não é wake do pacer
        assert_eq!(p.spin_ns, antes);
    }

    /// Diagnóstico manual: mede o atraso de wake do clock_nanosleep em
    /// cadência de 30 fps, com margens de spin diferentes. Rodar com:
    /// cargo test -p bitu-layer --release diagnostico_wake -- --ignored --nocapture
    #[test]
    #[ignore]
    fn diagnostico_wake() {
        let alvo = 33_333_000i64;
        for spin_ms in [1i64, 2, 3, 4] {
            let mut p = pacer_teste(alvo, spin_ms * 1_000_000);
            let mut deadline = now_ns();
            let mut lates = Vec::new();
            for _ in 0..60 {
                deadline += alvo;
                let _ = p.sleep_until(deadline);
                lates.push((now_ns() - deadline).max(0));
            }
            lates.sort_unstable();
            let n = lates.len();
            let max = lates[n - 1];
            let p99 = lates[((n as f64 * 0.99).ceil() as usize).saturating_sub(1).min(n - 1)];
            let tardios = lates.iter().filter(|&&v| v > 100_000).count();
            println!(
                "spin {} ms: max {:.3} ms, p99 {:.3} ms, >0,1 ms: {}/{}",
                spin_ms,
                max as f64 / 1e6,
                p99 as f64 / 1e6,
                tardios,
                n
            );
        }
    }

    /// Diagnóstico manual do caminho completo (frame_enter + adaptação):
    /// cargo test -p bitu-layer --release diagnostico_adaptativo -- --ignored --nocapture
    #[test]
    #[ignore]
    fn diagnostico_adaptativo() {
        let alvo = 33_333_000i64;
        let mut p = pacer_teste(alvo, SPIN_MIN_NS);
        let mut prev = 0i64;
        let mut fts = Vec::new();
        for i in 0..240 {
            let antes = now_ns();
            p.frame_enter();
            let depois = now_ns();
            if i > 0 {
                fts.push(depois - prev);
            }
            prev = depois;
            if i == 0 {
                assert!(antes > 0);
            }
        }
        fts.sort_unstable();
        let n = fts.len();
        let max = fts[n - 1];
        let p99 = fts[((n as f64 * 0.99).ceil() as usize).saturating_sub(1).min(n - 1)];
        let tardios = fts.iter().filter(|&&v| v > alvo + 100_000).count();
        println!(
            "adaptativo: spin final {:.3} ms, pico {:.3} ms, ft max {:.3} ms, p99 {:.3} ms, >alvo+0,1: {}/{}; entrada_tarde={} acordou_tarde={} spin_tarde={}",
            p.spin_ns as f64 / 1e6,
            p.diag.pico_acordo_ns as f64 / 1e6,
            max as f64 / 1e6,
            p99 as f64 / 1e6,
            tardios,
            n,
            p.diag.entrada_tarde,
            p.diag.acordou_tarde,
            p.diag.spin_tarde
        );
    }
}

/// Ponto de entrada único chamado por Vulkan e GL a cada frame.
#[inline]
pub fn frame() {
    if let Ok(mut p) = pacer().lock() {
        p.frame_enter();
    }
}
