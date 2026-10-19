# Notas de desenvolvimento — Bitu

Última sessão: **Fase C concluída e validada E2E por código — GLX 3/3,
Vulkan nativo 3/3, simulação de sandbox 4/4 — e no jogo real
(Spider-Man/Proton, DXVK): 30 fps estáveis com alvo de 33,333 ms vindo da GUI**.
Hook Vulkan corrigido para o present REAL do ICD (o export do loader e o stub
de `vk_icdGetInstanceProcAddr` não são a função chamada por DXVK/winevulkan).
Manifest implícito corrigido (faltava `disable_environment`; o loader o
ignorava). Próximo passo: Fase B (OSD).

## GUI redesenhada (19/10/2026)
- Janela fixa **640x180 (32:9)**, sem redimensionar nem maximizar; fundo
  preto, cartões cinza, textos em **en-US** (a UI não é mais PT-BR).
- Três cartões no topo: título (`Bitu v1.0.0` + "frame limiter" + botão de
  controle), telemetria (`fps`/`avg fps`, `FT`/`avg FT`, janela de 600
  amostras) e alvo (`frame time by app`: 33,33 ms / 30 fps + PID anexado).
- Gráfico grande embaixo só com cores (verde/amarelo/laranja/vermelho) e a
  linha do alvo, sem rótulos de texto. As causas (pacer/jogo) continuam na
  amostra e definem a cor; p99/máx/% saíram da UI.
- Linha do FT desenhada como **polilinha contínua por trecho de cor** (um
  `Shape::line` por trecho, não 1 segmento por amostra): antes, as emendas de
  antialias a cada ~1 px deixavam a linha do alvo aparecer por baixo e a linha
  parecia tracejada quando o FT ficava exatamente no alvo.
- Cartões com layout vertical forçado (o conteúdo herdava o horizontal da
  fileira e o texto do 3º cartão vazava/cortava).
- Alvo fixo em **30 fps / 33,333 µs** (sem presets nem campo de ms). Saíram
  também o modo lançar e o instalador de manifest da GUI; o anexo é pelo
  popup do botão de controle (lista de PIDs, **attach**/**detach**).
- Versão do app: **1.0.0** (o título `Bitu v1.3.0` era resquício; o Cargo já
  era 1.0.0 e o `bitu-inject/Cargo.toml` foi alinhado de 1.3.0 → 1.0.0).
  Referências a v1.3.0 abaixo são histórico de fases, não a versão atual.
- Validação sem tela: `cargo test -p bitu-gui` **11/11** (render headless em
  640x180, médias da janela, cores por causa, polilinha contínua do gráfico e
  textos sem corte); `./build.sh` sem warnings; GUI real loga
  `bitu: janela fixa 640x180 ok`.

## Fase C — anexar em app já rodando (v1.3.0)

### O que foi construído
- `bitu-inject/` (novo binário): injetor ptrace+dlopen — parser ELF64 dynsym
  próprio (sem dependências), hijack de registradores com retorno via "ret
  para 0" (SIGSEGV interceptado e descartado pelo ptrace) e
  `orig_rax = u64::MAX` obrigatório antes de cada CONT (o kernel cancelava o
  "syscall restart" e reescrevia RIP-2 — bug real). Modos: `<pid> <lib>` e
  `--spawn <lib> <cmd>` (injetor vira pai do alvo — único que passa com Yama
  scope=1).
- `bitu-layer/src/inject.rs` (novo): `bitu_inject_install()` exportada,
  chamada pelo injetor já dentro do alvo. Resolve o endereço real com
  `dlopen(RTLD_NOLOAD)+dlsym` por lib candidata **com guarda `er_nosso()`**
  (`dlsym(RTLD_DEFAULT)` dentro da layer retorna os exports DA PRÓPRIA layer —
  bug real); fallback: /proc/maps + dynsym do arquivo. Planta `int3` no 1º
  byte (escrita de 1 byte é atômica; página vira RWX) e instala o handler de
  SIGTRAP **antes** de qualquer patch.
- GUI: lista de processos do usuário (uid + cmdline de `/proc`, conectados
  primeiro), botão **anexar** (dispara o injetor em thread + mpsc) e
  **desligar**.

### Bug crítico encontrado e corrigido (causa-raiz via core dump)
Sintoma: o app morria silenciosamente no 1º present pós-injeção (nenhum log
`[bitu]`, `pacing::frame()` nunca rodava).

Diagnóstico (fontes: `journalctl -k`, `coredumpctl dump <pid>`, gdb no core):
- Kernel: `segfault at 0 ... error 6 in libbitu_layer.so`; core:
  `#0 inject::ao_trap+0xb5` (`mov %al,(%r15)` com r15=0), `#2 glXSwapBuffers+1`.
- **Semântica real do kernel x86-64**: `int3` de userspace entrega SIGTRAP
  com `si_code = SI_KERNEL (0x80)` e `si_addr = 0` — **não** `TRAP_BRKPT` com
  o endereço. O handler antigo fazia `addr = si_addr` (=0) contra os slots e o
  slot VAZIO (sentinela `addr==0`) casava → `write_volatile(0, ...)` →
  SIGSEGV dentro do handler → morte silenciosa.
- O endereço do trap vem do RIP do ucontext: int3 é trap de 1 byte, então
  `RIP == hook + 1`.

Correção (`inject.rs`), sem confiar em `si_code`/`si_addr`:
- int3 identificado por `rip - 1` (e `si_addr` se != 0); slot com `addr == 0`
  nunca casa; confere que o byte no endereço ainda é `0xCC` antes de mexer.
- single-step identificado pelo thread-local `PASSO` (não por `si_code`).
- **Pré-aquecimento** em `bitu_inject_install` antes do 1º int3:
  `PASSO.with(set 0)` (materializa TLS dinâmico desta thread) +
  `pacing::pre_aquecer()` (env/CString/OnceLock) — evita malloc no contexto
  de sinal (malloc não é async-signal-safe).

### E2E da Fase C (tudo por código, headless)
`bitu_serv` (falso-GUI; janelas 0/33333/41667/0 µs) + injeção ao vivo:
- **GLX** (`bitu_glx`, `BITU_GVSYNC=0`): 4/4 PASS — 0,06 / **33,33** /
  **41,67** / 0,06 ms; app vivo (201.925 frames, ft 0,10 ms médio).
- **Vulkan** (`bitu_vk` novo; sem manifest, `HOME` falso p/ excluir a layer
  implícita): 4/4 PASS — 0,13 / **33,33** / **41,67** / 0,16 ms. Hook
  confirmado no log: `vkQueuePresentKHR via libvulkan.so.1` (byte 0xf3→0xcc),
  `pid … ok 4`.
- `./build.sh` limpo; `cargo test` ok (gui 4/4, layer 1/1); stderr do app sem
  ruído fora de `BITU_DEBUG=1`.

### Suporte a jogos em sandbox (Flatpak/Proton) — 19/10
Diagnóstico no jogo do usuário (Spider-Man via Faugus/Proton):
userns próprio (uid 1000→1000), mountns próprio com o runtime em `/run/parent`,
**netns compartilhado**; `XDG_RUNTIME_DIR` filtrado (sem `bitu/`).
- **Injetor**: `ler_elf()` tenta o caminho direto e, se não existir no nosso
  mountns (caso `/run/parent/...`), lê por `/proc/<pid>/root/<caminho>`.
  Usado na libc e no `.so` da layer.
- **IPC**: socket abstrato `\0bitu-<uid>` como primeiro alvo — atravessa
  sandbox no mesmo netns; o arquivo fixo vira fallback. A GUI escuta nos dois
  (`ss -xl`: `@bitu-1000` + `bitu.sock`), com `SO_PEERCRED` mantido.
- **Validação por simulação de sandbox** (userns 1000→1000 + mountns privado
  com tmpfs; libc/libvulkan/app só visíveis dentro): injeção ok lendo a libc
  via `/proc/<pid>/root`, `ok 4`, conexão `@bitu-1000` e 4/4 PASS
  (2,09 / **33,33** / **41,67** / 2,35 ms).
- E2E nativo revalidado: GLX abstrato 4/4, Vulkan abstrato 4/4, regressão pelo
  arquivo (`BITU_SOCK`) GLX 4/4.
- Nota: um stall de ~1 s na 1ª amostra do Vulkan era o
  `vkAcquireNextImageKHR` do app de teste (timeout de 1 s) drenando o backlog
  do Xwayland após 2 s a ~5700 fps — não é a layer. O `bitu_serv` agora
  descarta 1 s de assentamento por janela.
- Teste no jogo real: ver a seção "Teste no jogo real (Spider-Man/Proton)" —
  PASS em 19/10 (30 fps com alvo da GUI).

### Correção do hook Vulkan para DXVK/Proton — 19/10 (sessão atual)
Sintoma: no jogo (DXVK) o `int3` era gravado, mas nenhum present disparava; no
app nativo (chamada direta do export) funcionava.
- Diagnóstico com gdb/ptrace: o export `vkQueuePresentKHR` do loader faz
  `mov (%rdi),%rax; cmp magic; jmp *disp(%rax)` — ou seja, **salta pela
  dispatch table da fila**. O ponteiro que o DXVK usa vem de
  `vkGetDeviceProcAddr`, outro trampolim do mesmo loader, e também termina
  nessa tabela. O export hookado não é o alvo do salto.
- `vk_icdGetInstanceProcAddr(inst, "vkQueuePresentKHR")` devolve um stub
  genérico (`f3 0f 1e fa 48 8b 47 10 ff a0 f0 08 00 00`), **não** a função WSI
  que o loader chama; hookar nele não gera frames.
- A entrada real (gdb): começa com
  `f3 0f 1e fa 48 8b 4f 10 48 89 f2 48 89 fe 48 8b 49 70 48 8b b9 c0 15 00 00`
  (stub WSI do ICD). A tabela tem magic `0x10aded040410aded` em `[0]`; o
  índice do present é o `disp` do `jmp` do export (0x690 no loader 1.4.36x,
  calculado em runtime, não hardcoded).
- **Método novo** (`inject.rs::presents_vulkan()`): cria uma instância pelo
  **loader** com `VK_KHR_surface` + xlib/xcb/wayland e um device mínimo com
  `VK_KHR_swapchain` (sem swapchain), pega a fila, lê a entrada da dispatch
  table e valida o magic. Repete para cada physical device (cobre 2 GPUs/ICDs),
  ignora `er_nosso()` e deduplica. Fallback: export do loader. A sonda roda
  numa thread própria ~200 ms depois do attach (a thread principal está parada
  no ptrace; criar instância/device nela poderia travar em lock do driver).
- `ALVOS` 8 → 16 (3 WSI × physical devices + GL/EGL). Bit 4 do mask continua
  otimista; a sonda instala o hook depois.
- Validações pós-correção (medições sem carga):
  - Vulkan nativo (`bitu_vk` com `DISABLE_BITU_LAYER=1`, IPC por arquivo):
    W0 0,15 ms; W1 **33,33**; W2 **41,67**; W3 0 — PASS. Log:
    `present VK_KHR_xlib_surface = …libvulkan_radeon.so` e `hook … 0xf3->0xcc`.
  - GLX nativo: 0,07 / **33,33** / **41,67** — PASS.
  - Simulação de sandbox (`sbx_test.sh`, tmpfs privado, userns 1000→1000):
    4/4 PASS; injeção lendo libc/layer por `/proc/<pid>/root`; IPC abstrato.
- **Bug do manifest descoberto nesta sessão**: o loader exige
  `disable_environment` em manifest implícito; o JSON gerado pela GUI não
  tinha, então o loader o **descartava** (`WARNING: Layer "VK_LAYER_BITU_1"
  doesn't contain required layer object disable_environment`). Corrigido no
  gerador (`bitu-gui/src/main.rs`) e em `bitu-layer/bitu_layer.json`, e o
  arquivo do usuário foi reescrito. Confirmado: o loader agora insere
  `VK_LAYER_BITU_1`, e um `bitu_vk` sem injeção conectou em `@bitu-1000` na
  GUI. `DISABLE_BITU_LAYER=1` desliga (usado nos testes de injeção).
- Manifest em Proton continua **ineficaz**: o pressure-vessel define
  `VK_IMPLICIT_LAYER_PATH` (substitui os caminhos padrão) e remove
  `VK_ADD_IMPLICIT_LAYER_PATH`; o jogo precisa do modo **anexar**.
- Pendente: teste no jogo real — o processo antigo (pid 59700) foi fechado;
  é preciso reabrir Spider-Man e clicar **anexar** na GUI.

### Teste no jogo real (Spider-Man/Proton) — 19/10, PASS
Usuário reabriu o jogo pelo Faugus/Proton-CachyOS e anexou pelo botão da GUI.
- Processo: `Spider-Man.exe` (host pid 81699); `libbitu_layer.so` mapeada.
- IPC: `@bitu-1000` **ESTAB** entre o jogo (fd 585) e a GUI (fd 16); o accept
  só aceitou porque o PID estava selecionado.
- Estado do `Pacer` lido **read-only** de `/proc/81699/mem` (símbolo `PACER`
  em `libbitu_layer.so` + base dos maps), sem parar o jogo:
  - `target_ns = 33.333.000` (30 fps) e `env_target_ns = 0` — o alvo veio
    **ao vivo da GUI** pelo socket (o jogo não tem `BITU_FPS` no env);
  - `ipc.fd = 585` (o mesmo do `ss`), `frames` incrementando.
  - Contagem por segundo: 31 / 29 / 31 / 30 frames → **~30,25 fps** com alvo
    30; gaps `last→anchor` ~32,5 ms no regime, com picos ocasionais de ~56 ms
    (frame perdido do próprio jogo, não do pacer).
- Nenhum screenshot/gravação; validação por socket, contadores e leitura de
  memória. O jogo carrega `libvulkan_lvp.so` e `libvulkan_radeon.so` com loader
  `1.4.357`; a sonda calculou o `disp` em runtime e instalou o hook na entrada
  da dispatch table. O jogo seguiu rodando normalmente.
- Observação: o jogo pode estar com vsync ligado; o alvo de 30 fps foi
  atingido com picos de 56 ms. Para o teste fino de 24/30/0, os E2E nativos já
  cobrem a troca ao vivo; no jogo o usuário pode usar **desligar** e trocar o
  alvo na GUI.

### Correção do frame time (pacer rígido) — 19/10, PASS
Pesquisa no RTSS/Afterburner: o limitador não recupera média; ele segura a
**liberação do frame** e usa spin de alta resolução. O `Pacer` antigo tinha
`corr = late/4` e só re-ancorava depois de `4 * alvo`, então um pico de 56 ms
gerava vários frames de ~27–31 ms para "compensar" — era essa a variação do
gráfico, não o jogo.
- `pacing.rs` reescrito para **intervalo mínimo**: `deadline = última liberação
  + alvo`. Chegou cedo → `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)` até
  `deadline - spin` + busy-wait até o deadline; chegou atrasado → libera agora
  e o próximo deadline parte da liberação real (sem encurtar o próximo frame).
- Margem de spin **adaptativa**: começa em 1 ms; se o nanosleep acordar depois
  do deadline, guarda `latência = atraso + spin usado`, sobe o spin para
  `latência + 300 µs` (teto de 5 ms) e reduz 1 µs a cada 64 frames sem atraso.
  Assim a oscilação de ~1–2 ms causada pelo wake da máquina some sem busy-wait
  fixo alto. O atraso de **entrada** (jogo) não mexe no spin — é outra causa.
- IPC da amostra passou de 8 → 12 → 16 e agora **20 bytes**: ft (µs),
  contador, atraso em relação ao deadline (µs), atraso de wake do pacer (µs)
  e **alvo usado no frame** (µs; 0 = sem limite). A GUI aceita 8, 12, 16 e 20
  — GUI e layer podem ser atualizadas em qualquer ordem. Com o 4º campo a GUI
  separa **pacer** (wake do nanosleep) de **jogo** (entrada depois do
  deadline); com o 5º, o gráfico e a telemetria só mostram o **regime atual**
  (mesmo alvo), sem misturar amostra de antes do limite ou de outro preset.
  Camada antiga manda alvo desconhecido (`u32::MAX`) e continua aparecendo.
- GUI: alvo principal em **ms** (fonte de verdade em µs; presets 30/60/120 fps),
  telemetria com p99/máximo/% no alvo (±1 ms) e gráfico em janela de ±(2–8 ms)
  em torno do alvo, com 600 pontos fixos. Cores por causa e gravidade:
  **verde** dentro de ±1 ms; **amarelo** desvio de 1–3 ms sem causa definida;
  **laranja** atraso do jogo (entrada depois do deadline) ou desvio >3 ms;
  **vermelho** atraso do pacer (wake ≥1 ms). Picos fora da escala levam risco
  no topo com a mesma cor. Os números usam a **mesma janela do gráfico (600
  amostras, ~20 s)**, então o `máx` bate com o pico desenhado; as linhas
  `pacer` e `jogo` ficam vermelha/laranja quando passam de 1 ms.
- Testes: `atraso_nao_encurta_o_proximo_frame` + `cadencia_estavel` +
  `spin_cobre_wake_tardio` (3/3 layer) + `diagnostico_wake` e
  `diagnostico_adaptativo` (ignorados, manuais) e 6/6 GUI.
- Diagnóstico ao vivo no Spider-Man (pid 105439, 30 fps): pacer acordou tarde
  3×, no máx **0,553 ms**, e a margem subiu para **2,69 ms** (pico aprendido
  2,39 ms) — a oscilação de 0,7–1,6 ms era o wake do pacer e foi coberta.
  O jogo atrasa a entrada em 3.416 frames (pico **290 ms**, carregamento/
  transição): no histórico, os 294 frames >39 ms têm `acordou=0` — o pico
  vermelho do gráfico era do **jogo**, não do limitador.
- Validação final no jogo real (Spider-Man, pid 111573, 30 fps, em gameplay):
  `pacer 0,00 ms` e `acordou=0` na janela inteira (o spin nem saiu do mínimo
  de 1 ms); a única espiga foi **laranja**, do jogo: frame de **42,55 ms**
  com `jogo 9,21 ms`, em gameplay no meio da partida. Conclusão: o limitador
  não oscila; o pico residual é trabalho do próprio jogo (fora do escopo —
  o jogo não será instrumentado nem alterado). Verde = no alvo; laranja =
  jogo; vermelho = pacer (não ocorreu).
- E2E nativo (`bitu_vk`, sonda + injeção, fake-GUI em arquivo):
  - W0 sem alvo: média 0,18 ms (sem limite);
  - W1 alvo 33333 µs: **33,333 ms** constante;
  - W2 alvo 41667 µs: **41,667 ms** constante;
  - W3 sem alvo: volta a 0,17 ms;
  - 0 janelas reprovadas; nenhum catch-up após troca de alvo.
- Limitações: se o jogo não consegue renderizar em ≤ alvo, o pacer não inventa
  frame (o gráfico e o `no alvo %` mostram isso). Com vsync ligado ainda pode
  haver vblank perdido (~50/56 ms); a sincronização de vblank fica para uma
  fase separada, se o usuário quiser.

### Pendências Fase C
- [x] Teste em jogo real com o anexo (Steam/Proton): Spider-Man via
  Faugus/Proton-CachyOS (pid 81699) — layer + socket + **30 fps** com alvo da
  GUI; ver a seção "Teste no jogo real" acima.
- [ ] vsync do jogo ligado pode deixar o alvo um pouco acima (espera dupla) —
  recomendação: vsync off (igual RTSS).
- [ ] Fluxo visual da GUI (clicar anexar/desligar num app real) — lógica
  validada por unidade + E2E; falta o olho humano.
- [ ] Wayland nativo (sem XWayland) ainda não testado.
- [ ] Jogos 32-bit não cobertos: layer e injetor são 64-bit.

### Validações extras feitas nesta sessão
- Caminho de erro do modo 1 sem o sysctl: `bitu-inject <pid> <lib>` num
  processo que NÃO é filho → `erro: permissão negada (ptrace bloqueado).
  Libere com: sudo sysctl kernel.yama.ptrace_scope=0` (rc=1) — é exatamente o
  que a GUI mostra no status ao clicar anexar nessas condições.
- GUI (v1.3.0) aberta na sessão do usuário: listen confirmado em
  `/run/user/1000/bitu/bitu.sock` (`ss -xl`), sem erros no log.

### Correção pós-validação (medidas sob carga)
- No meio da sessão o usuário abriu um jogo (Proton) — load ~7. Os E2E que
  rodaram DEPOIS disso deram números degradados (41,66 ms com alvo 33,33) e
  um travamento do `bitu_vk`. Causa do travamento: sob carga o
  `vkAcquireNextImageKHR` estourou o timeout de 1 s e o APP DE TESTE (C)
  seguia para `vkQueueSubmit` com o semáforo não sinalizado →
  `vkQueueWaitIdle` eterno. **Bug do app de teste, não da layer**; corrigido
  (no TIMEOUT não submete). Os 4/4 válidos (GLX e Vulkan) foram medidos antes
  do jogo (11:27–11:36).
- Jogo do usuário (Spider-Man Remastered via Proton): o manifest implícito
  **não** entra no sandbox (ver a seção da correção do hook Vulkan); por isso
  o caminho é **anexar**. O path do `.so` é visível dentro do container do
  Steam Runtime (`/proc/<pid>/root/home/usr/...` existe), então o anexo por
  PID deve funcionar com `kernel.yama.ptrace_scope=0`.

## Validação da Fase A (feita 19/10/2026, tudo headless, sem contato com a UI)

1. **Unidade — gate por PID na GUI (4/4 ok, `cargo test -p bitu-gui`)**:
   `gate_por_pid`, `troca_de_sel`, `sem_sel_ninguem_limitado`,
   `fd_sem_pid_nunca_recebe_alvo` — verificam os BYTES reais enviados por
   `redistribuir_alvo()` (extraído do método Bitu para função testável) via
   `socketpair`: selecionado → alvo; resto → 0.
2. **E2E — layer com alvos aplicados ao vivo (servidor teste `bitu_serv` +
   app `bitu_glx`, janela glx de 320×240, swapinterval 0 via BITU_GVSYNC=0)**:
   - W0 sem alvo: média 0,06 ms (sem seleção limita nada) PASS
   - W1 alvo 33333µs (30fps): média **33,33 ms** exata, gaps 33,29–33,45 PASS
   - W2 alvo 41667µs (24fps): média **41,67 ms** exata PASS
   - W3 alvo 0 (desliga): média 0,06 ms PASS
   Amostras individuais do pacer têm erro < 0,2 ms da grade — cadência console.
3. **Anexo sem env**: layer (`LD_PRELOAD` ou manifest) conecta sozinha no
   caminho fixo `$XDG_RUNTIME_DIR/bitu/bitu.sock` e aparece parada até ser
   selecionada (validado no dia 18: "ipc conectado" + 16,79 ms vsync intacto).
4. **Manifest**: botão instalar da GUI validado com clique do usuário (18/10);
   arquivo em `~/.local/share/vulkan/implicit_layer.d/bitu_layer.json` com
   `library_path` absoluto correto — porém só a **presença do arquivo** tinha
   sido conferida. Em 19/10 o loader foi testado com `VK_LOADER_DEBUG=layer` e
   **descartava** o manifest por falta de `disable_environment`; corrigido no
   gerador e no arquivo do usuário (ver a seção de 19/10).

### Pendências Fase A (baixas, não-bloqueantes)
- [ ] Modo "lançar" da GUI pós-refatoração (o caminho de pacing env-side é o
  mesmo E2E já validado; falta só o fluxo de spawn da UI).
- [ ] Telemetria por PID na UI com 2 apps ao mesmo tempo (a lógica por fd está
  testada em unidade; falta observação visual).
- [ ] Jogo Steam/Proton real (DXVK) — inclusive **com vsync do jogo ligado**:
  o pacer funciona, mas com vsync do jogo ativo há waits duplos (o swap bloqueia
  no vblank deposis do sono do pacer) — deve ficar igual ou um pouco acima do
  alvo; recomendar desligar vsync do jogo (mesmo conselho do RTSS).
- [ ] Van a validar em Wayland nativo (apps Vulkan nativos) sem XWayland.

### Histórico do 18/10 (parcialmente explicado)
O teste interativo de ontem rendeu ft médio 39,95 ms em ~4 min — consistente
com pacer aplicado com vsync do jogo ligado (waits duplos em fases alternadas);
com swapinterval 0 (hoje) o alvo fica EXATO. Avaliar expor dica na UI:
"em jogos com vsync ativo o limiar pode subir um pouco; desligue o vsync do jogo".

## Ferramentas de teste (recriadas hoje em /tmp/opencode/)
- `bitu_glx.c` → janela GLX com vsync configurável:
  `BITU_GVSYNC=0 bitu_glx 20s` (por tempo) ou `bitu_glx 900` (por frames);
  imprime "ft medio" no fim. Compilar: `gcc -o bitu_glx bitu_glx.c -lX11 -lGL -lm`.
- `bitu_vk.c` (Fase C) → app Vulkan mínimo X11 com **declarações manuais**
  (sem headers Vulkan no sistema): janela 320×240, present IMMEDIATE, loop
  acquire→submit(barrier)→present→queue wait. `bitu_vk 20` (segundos).
  Compilar: `cc -O2 -Wall -o bitu_vk bitu_vk.c -lvulkan -lX11`.
  Para excluir a layer implícita do teste (provar que é a injeção):
  `DISABLE_BITU_LAYER=1` no ambiente do app (ou `HOME=/tmp/opencode/fakehome`,
  que esconde o manifest do usuário).
- `bitu_serv.c` → falso-GUI E2E: bind no caminho fixo, janelas de alvo
  programadas e estatísticas por janela (média/min/max/n + PASS/FAIL);
  exit code = nº de reprovadas. `_GNU_SOURCE` necessário para `ucred`.
  Compilar igual; rodar ANTES do app, em background.
- Dica: limitar por TEMPO não por frames quando fase inicial é sem limite
  (aplicações se aceleram a ~0,05 ms/frame e comem a fila de frames).
- Dica: crash de injeção deixa core (systemd-coredump):
  `coredumpctl list` → `coredumpctl dump <pid> -o core` →
  `gdb <bin> core` (o core tem o siginfo ORIGINAL do trap no frame de sinal —
  foi assim que a semântica SI_KERNEL/si_addr=0 do int3 apareceu).
- Dica: `pkill -f bitu_serv` mata o PRÓPRIO shell (o cmdline contém o padrão);
  use `pkill -x bitu_serv`.

## Estado atual (v1.3.0 — Fase C feita e validada; Fase A mantida)

### O que mudou nesta sessão

**`bitu-layer/src/pacing.rs`**
- `sock_padrao()`: sem `BITU_SOCK`, a layer conecta sozinha em
  `$XDG_RUNTIME_DIR/bitu/bitu.sock` (com fallback /tmp) — é o que habilita o
  anexo por manifest implícito.
- `IpcClient::poll_target()` retorna enum `PollAlvo { Nada, Alvo(u32), Caiu }`
  (no nível do módulo, não dentro do impl).
- Pacer ganhou `env_target_ns`: quando a GUI cai (`PollAlvo::Caiu`), o alvo
  volta ao valor do env (`BITU_FPS`, normalmente 0 = inerte).
- `frame_enter()` tenta `try_connect` sempre (não só com alvo>0) — sem isso o
  app nunca apareceria na lista da GUI.
- Re-âncora (`anchor_ns = 0`) ao trocar de alvo para não pular cadência.

**`bitu-layer/src/vulkan.rs`**
- `eprintln` do "chain info não encontrado" agora só com `BITU_DEBUG=1`
  (layer implícita carrega em todo app Vulkan; não pode poluir stderr).

**`bitu-gui/src/main.rs`**
- Socket fixo `bitu.sock` (sem PID no nome — launch-options do Steam estáveis).
- `Compartilhado.samples_us` agora é `VecDeque<(pid, ft_us)>` — amostras
  marcadas por app (2+ apps Vulkan conectados não se misturam mais).
- `Compartilhado.alvo_pid: Mutex<i32>` (0 = nenhum) — fonte única de seleção,
  lida pela thread IPC.
- Gating por conexão: `enviar_alvo_por_fd()` manda `alvo_us` só para o fd do
  PID selecionado e **0** para os demais (a layer mede, mas não limita).
  O accept no servidor também checa `cred.pid == selecionado`.
- App que sai: thread IPC zerar `alvo_pid` se for o selecionado (fallback
  duplicado em `atualizar_lista()`).
- UI: lista de apps conectados (PID · nome · cmdline de `/proc`) com busca,
  auto-refresh 500 ms (`prox_refresh`), clique = ativa/desativa,
  botão "desligar limite"; caixa "app limitado" no lugar de "jogo".
- Botões instalar/remover manifest em `~/.local/share/vulkan/implicit_layer.d/
  bitu_layer.json` (`instalar_manifesto()` escreve JSON com `library_path`
  absoluto para o `.so` junto ao binário; `manifesto_instalado()` checa).
- Telemetria corrigida: "—" sem dados; "atual %" = fps instantâneo ÷ alvo;
  linha "ft alvo" em ms; gráfico/telemetria filtram por PID selecionado.
- Lançar via GUI: app nasce selecionado automaticamente (`alvo_pid` setado no
  spawn). Backlog do listener 8 → 64.

### Validação feita nesta sessão
- Build limpo (`./build.sh`), `cargo test -p bitu-layer` ok (cadência).
- GUI rodando → socket fixo criado (`ss -xlp` confirma listen) ✓
- `bitu_glx` com **env limpo** + `LD_PRELOAD` da layer nova:
  "ipc conectado" nos logs e **16.79 ms** (vsync) — conectou pelo caminho
  padrão e ficou **inerte** (não selecionado) ✓

### Validação pendente (superseded — ver seção "Validação da Fase A" acima)
- ~[ ] Pacing de fato quando o app é selecionado na GUI~ → feito por código
  (unidade + E2E com alvos 30/24 fps e desligar, td PASS).
- ~[ ] Botões instalar/remover manifest via clique~ → instalar confirmado
  (18/10, clique do usuário + inspeção do arquivo). Falta remover (trivial).
- ~[ ] Modo lançar pós-refatorar~ → segue pendente (baixa prioridade).
- ~[ ] Telemetria por PID na UI com 2 apps conectados de uma vez~ → segue pendente.
- [ ] Após validado: testar num jogo Steam/Proton real (DXVK).

## Próxima fase — Fase B: OSD (overlay no jogo) — ainda NÃO iniciada
- Interceptar mais funções em `vulkan.rs` (estender `DeviceData` + `N_*`):
  `vkCreateSwapchainKHR` (pegar formato/extent), `vkAcquireNextImageKHR/2`,
  `vkDestroySwapchainKHR`.
- Desenhar texto (fps / ft / %) na imagem adquirida antes do `vkQueuePresentKHR`
  (mangoHud/RTSS-style: transição de layout em volta do próprio render pass,
  usando a queue do jogo — `QUEUES` map já existe).
- GLSL→SPIR-V em runtime via crate `naga` (puro Rust).
- Toggle na GUI; só desenhar no PID selecionado.
- Primeira versão: só texto; mini-gráfico depois.

## Limitações aceitas (não tentar "resolver" sem discutir)
- App já rodando sem manifest: agora coberto pelo anexo (Fase C, ptrace) —
  requer `kernel.yama.ptrace_scope=0`.
- Anexo não tem "desanexar": desligar o limite manda alvo 0 e os hooks ficam
  inertes (sem custo relevante).

## Gotchas de ambiente (relembrete)
- Build da layer SEMPRE por `./build.sh` (tem `-Wl,-Bsymbolic` — sem isso o
  loader interpoia nossos símbolos e quebra) e `CARGO_HOME=$PWD/.cargo`.
- `-Bsymbolic` só vale para a layer; não usar no build da GUI (build.sh já
  cuida disso com unset).
- Testes E2E atuais em `/tmp/opencode/`: `bitu_glx` (GLX/XWayland),
  `bitu_vk` (Vulkan, declarações manuais), `bitu_serv` (falso-GUI).
  Os antigos (`bitu_vktest`, `bitu_vkdev`, headers) foram apagados.
- `bitu_glx` no meu caso de teste precisa de `DISPLAY=:0` (XWayland).
- Manifest implícito do usuário é carregado para QUALQUER app Vulkan do
  sistema — qualquer mudança na layer é efetivada pelo rebuild do `.so`
  (mesmo caminho absoluto), não precisa reinstalar. Use
  `DISABLE_BITU_LAYER=1` para desligá-lo num app/teste. Em Proton/Flatpak o
  pressure-vessel ignora o manifest (ver seção de 19/10).
- Artefatos velhos de socket `bitu-<pid>.sock` em `$XDG_RUNTIME_DIR/bitu/`
  das versões antigas — pode limpar; a versão nova usa só `bitu.sock`.

## Plano de Fase B detalhado (para retomar)
1. `vulkan.rs`: adicionar hooks e mapeamentos (padrão atual de
   `read_device_chain`/`N_*`; coerção de wrapper via `from_void`/`to_void`).
2. `osd.rs` novo módulo: estado por device (pipeline, descriptor, font —
   bitmap embutido 8px do jeito simples, SPIR-V vindo do naga).
3. Caminho de desenho: no `my_queue_present`, para cada swapchain em uso,
   transicionar IMAGEM do jogo UNDEFINED→COLOR_ATTACHMENT (via
   `vkCmdPipelineBarrier` num command próprio... AVISO: commands precisam de
   command pool própria — alocar no init per-device amostrando uma queue),
   render, transicionar para PRESENT_SRC, depois chamar `next_present`.
   Complexidade real: a imagem pode já estar em PRESENT_SRC — manejar com
 barriers (mesmo padrão de overlay hacks).
4. IPC: mensagem extra GUI→layer "osd on/off" (protocolo de 8 bytes atual:
   estender p/ 12 bytes {alvo_us, flags} — mudar os DOIS lados juntos).
5. Testar com `bitu_vktest`/validação própria, depois jogo real.
