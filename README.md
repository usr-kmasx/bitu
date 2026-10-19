# Bitu v1.0.0

Console-style **frame-time** FPS limiter for Linux, like MSI Afterburner/RTSS
does on Windows. No third-party projects: the pacing layer, the injector and
the GUI are 100% this project.

## How it works

- `bitu-layer/` — library that runs inside the app (`libbitu_layer.so`):
  - **Vulkan implicit layer** (via the Vulkan loader + manifest): hooks
    `vkQueuePresentKHR` and enforces the **minimum interval** between presents
    at the target (`last release + target`), with no catch-up: if the game
    misses a frame, the next one gets a full period, like on a console. The
    wait uses absolute `clock_nanosleep` + busy-wait on the last 1 ms (µs
    precision).
  - **OpenGL hook** (`LD_PRELOAD`): hooks `glXSwapBuffers`/`eglSwapBuffers`
    with the same pacing core.
  - **Live hook** (attach to an already-running app): the injector plants an
    `int3` breakpoint at the first byte of the **real** present and runs the
    same pacing inside the live process — each present costs 2 traps (µs), with
    no app restart and no Steam needed:
    - Vulkan: the **ICD** WSI function, found through the loader's dispatch
      table (the `libvulkan.so.1` export and the pointer returned by
      `vkGetDeviceProcAddr` are not the function DXVK/winevulkan calls);
    - GL/EGL: the `glXSwapBuffers`/`eglSwapBuffers` exports.
  - Covers native Vulkan/GL apps **and** games through DXVK/VKD3D
    (Proton/Steam), because the present of all of them ends in the ICD
    implementation. The probe creates a minimal Vulkan instance/device on a
    short thread inside the target (~200 ms after attach) to discover that
    function; it is a one-time hitch.
  - Talks to the GUI over a Unix seqpacket socket: it tries the **abstract**
    socket `@bitu-<uid>` first (goes through Flatpak/Proton sandboxes that
    filter `/run/user`) and falls back to the fixed path
    `$XDG_RUNTIME_DIR/bitu/bitu.sock` (compatibility/tooling). It sends the
    release frame time, the counter and the lateness relative to each frame's
    deadline; it receives target/activation **live**. The GUI (en-US) shows
    current/average fps and frame time over a ~20 s window and a colors-only
    graph (green/yellow/orange/red); the target is fixed at
    **30 fps / 33.33 ms**.
  - **Inert by default**: connected or not, it only limits the PID selected in
    the GUI; with no active limit it is a zero-cost fast path. When the GUI
    exits, it falls back to the env target (`BITU_FPS`, if set) — usually 0.

## Three usage modes

### 1. Attach to an already-open app (GL and Vulkan)
In the GUI: click the gamepad icon (on the title card) and pick the process in
the PID popup; **attach** injects the layer (`ptrace` + `dlopen`) and plants
the present hooks in real time. **detach** goes back to no limit. Works for any
user app, opened before or after. The target is always 30 fps.

Kernel requirement (once, as root):
```sh
sudo sysctl kernel.yama.ptrace_scope=0
```
Without it the kernel only allows ptrace on child processes (`ptrace_scope=1`
is the hardened default) and the injector refuses with this hint.

It also works with games in **Flatpak/Proton** (pressure-vessel): the injector
reads the runtime libraries through `/proc/<pid>/root` when the path seen in
`/proc/<pid>/maps` (`/run/parent/...`) does not exist in the user namespace,
and the layer finds the GUI through the abstract socket (same network
namespace).

### 2. Attach by PID via manifest (Vulkan apps that open later)
The project manifest can be installed by hand (once), with the absolute
`library_path`:
```sh
mkdir -p ~/.local/share/vulkan/implicit_layer.d
sed "s#\.\./target/release/libbitu_layer\.so#$PWD/target/release/libbitu_layer.so#" \
    bitu-layer/bitu_layer.json > ~/.local/share/vulkan/implicit_layer.d/bitu_layer.json
```
From then on:

- **Every Vulkan app opened after the install** loads the layer automatically
  (MangoHud style, no launch options) and shows up in the GUI PID popup with
  **PID + name + cmdline** (read from `/proc`). In Proton/Flatpak games the
  manifest is ignored by the sandbox — in those cases use mode 1 (note below).
- Selecting the app in the popup = enabling the limit for that PID;
  **detach** = disable.
- Other connected apps keep sending telemetry, but are **not** limited (the
  GUI sends target 0 per connection).
- To stop loading into new apps, delete the installed manifest.

> **Proton/Flatpak:** inside the pressure-vessel sandbox the manifest is **not**
> loaded — the runtime sets `VK_IMPLICIT_LAYER_PATH` (which replaces the
> default paths) and removes `VK_ADD_IMPLICIT_LAYER_PATH`. For games in those
> sandboxes use mode 1 (**attach**), which injects the layer into the live
> process. To skip the implicit layer in native apps, set
> `DISABLE_BITU_LAYER=1`.

### 3. Launch manually (fallback: native GL app / no manifest)
The GUI does not launch processes; the manual path injects the layer into the
environment (`LD_PRELOAD` + `VK_LAYER_PATH`/`VK_INSTANCE_LAYERS` + `BITU_FPS`).

Steam/Proton manually, in the terminal:
```sh
env LD_PRELOAD="$(pwd)/target/release/libbitu_layer.so" \
    VK_LAYER_PATH="$(pwd)/bitu-layer" \
    VK_INSTANCE_LAYERS=VK_LAYER_BITU_1 \
    BITU_FPS=30 \
    steam -applaunch <appid>
```

### Validated (2026-10-19)
- **Real game:** Spider-Man Remastered via Faugus/Proton-CachyOS, attached
  through the GUI on `Spider-Man.exe`: `@bitu-1000` ESTAB, **33.333 ms**
  target coming live from the GUI, **~30 fps** measured (31/29/31/30 per
  second), with occasional 56 ms spikes from the game itself.
- Natives: GLX and Vulkan 3/3; Flatpak/Proton sandbox simulation 4/4. The
  DXVK/winevulkan path (`vkGetDeviceProcAddr`) was validated on the real game
  above, with the hook at the loader/ICD dispatch table entry.

## Build

```sh
./build.sh       # layer (-Bsymbolic) + GUI + injector
```

Binaries in `target/release/` (`bitu`, `libbitu_layer.so`, `bitu-inject`).

## Layer environment variables

| var | effect |
|----|--------|
| `BITU_FPS` | target in fps for manual/fallback mode; the GUI uses 30 fps |
| `BITU_SOCK` | forces a specific socket path; when absent, tries `@bitu-<uid>` then the fixed path |
| `BITU_DEBUG=1` | layer (`[bitu]`) and injector (`[inj]`/`[bitu-inj]`) logs |
| `DISABLE_BITU_LAYER=1` | prevents the implicit layer (manifest) from loading |

## Known limitations

- Live attach uses `ptrace`: requires `kernel.yama.ptrace_scope=0` and the same
  user's process. Anti-cheats may block ptrace — in that case use modes 2/3
  (manifest/manual launch), which do not depend on it.
- Layer and injector are **64-bit**: 32-bit games (some old Proton titles) are
  not covered.
- The abstract socket is valid within the same **network namespace**. Default
  Flatpak shares the network (Faugus/Spider-Man case); a sandbox with its own
  netns isolates any IPC and is not supported.
- The manifest only loads in Vulkan apps **opened after** the install (for
  already-open ones use mode 1). In Proton/Flatpak the manifest is ignored by
  pressure-vessel (see section 2) — use **attach**.
- Vulkan attach performs a one-time probe (~200 ms after attaching) that
  creates a minimal instance/device in the process to find the ICD present; on
  some drivers it may cause a short hitch, once.
- GL: apps that resolve `glXSwapBuffers` via `glXGetProcAddress` (rare) are not
  captured.
- With the game's vsync on the target may sit a bit above (double wait: vblank
  + pacer) — turn the game's vsync off, as with RTSS.
- Apps with 2 GPUs active at the same time use the pacing of the last device
  created (very rare case).
- No OSD (overlay) yet — planned as the next phase (phase B).

## Structure

```
bitu-layer/{src/{lib,pacing,vulkan,gl,inject}.rs, bitu_layer.json}  # layer + manifest
bitu-inject/src/main.rs                                             # ptrace injector
bitu-gui/src/main.rs                                                # GUI (PID popup + telemetry)
build.sh                                                            # build with -Bsymbolic
NOTES.md                                                            # development notes (pt-BR)
```
