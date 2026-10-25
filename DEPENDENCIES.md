# Dependencies

What Studio needs to build and run, where each dependency is used in the code, and what has to change to port it to other Linux setups or other operating systems.

## How Studio uses external libraries

The release binary links **only glibc** (`libc`, `libm`, `libgcc_s`; check with `ldd target/release/studio`). Every other system library is **opened at runtime** with `dlopen` (the `libloading` crate). As a result:

* building needs no `-dev` packages and no C compiler, only the Rust toolchain;
* a missing library disables one feature instead of preventing start-up (no JACK → offline mode, no lilv → no LV2 plugins, no LAME → no MP3 export);
* porting means changing library *names and loaders*, not build scripts.

The one build-time workaround: `jack-sys` insists on finding `jack` through pkg-config even though it loads libjack at runtime. `build-support/pkgconfig/jack.pc` is an empty stub, and `.cargo/config.toml` adds it to `PKG_CONFIG_PATH`.

## Sound stack

```
Studio (JACK client "studio")
  │  JACK API: libjack.so.0, dlopen'd by the `jack` crate
  ▼
pipewire-jack  ── preferred: PipeWire's libjack, talks to the PipeWire daemon
   or jackd2   ── classic JACK server (start with --jackd)
  │
  ▼
PipeWire / JACK server ──► ALSA (kernel drivers) ──► audio interface
```

* **Selecting PipeWire:** `main.rs` behaves like `pw-jack`. If PipeWire's `libjack.so.0` exists (`/usr/lib/*/pipewire-0.3/jack`), the process re-executes itself with that directory first in `LD_LIBRARY_PATH`. `--jackd` skips this.
* **Latency:** `--latency N` sets `PIPEWIRE_LATENCY`. The status-bar buffer picker calls `jack_set_buffer_size`, which under PipeWire forces the quantum of the whole graph.
* **Ports:** Studio registers `master_out_L/R`, `trackN_in_L/R` (audio tracks) and `trackN_midi_in` (instrument tracks), and connects them to hardware ports.
* **Hardware support:** device drivers, sample rates and MIDI hardware are entirely the sound server's job. Studio only sees JACK ports.

## Runtime system libraries

| Library (soname) | Debian/Ubuntu | Fedora | Arch | Used by | Purpose | If missing |
|---|---|---|---|---|---|---|
| `libjack.so.0` (PipeWire's) | `pipewire-jack` | `pipewire-jack-audio-connection-kit` | `pipewire-jack` | `jack` crate → `engine.rs` | Audio and MIDI I/O | Offline mode: no sound or recording; editing and export still work |
| `libjack.so.0` (JACK2) + `jackd` | `jackd2`, `libjack-jackd2-0` | `jack-audio-connection-kit` | `jack2` | same | Alternative to PipeWire (`--jackd`) | as above |
| `liblilv-0.so.0` (+ `serd`, `sord`, `sratom`, `zix`) | `liblilv-0-0` | `lilv-libs` | `lilv` | `lv2.rs` | LV2 discovery and instantiation | No LV2 plugins; built-in devices still work |
| `libX11.so.6` | `libx11-6` | `libX11` | `libx11` | `lv2ui.rs`, also winit's X11 backend | Plugin GUI windows (XWayland on Wayland) | No plugin GUIs; generic parameter panels still work |
| `libmp3lame.so.0` | `libmp3lame0` | `lame-libs` | `lame` | `export.rs` | MP3 export | MP3 options are disabled; WAV still works |
| `libEGL.so.1`, `libGL.so.1` (libglvnd) + Mesa or vendor driver | `libegl1`, `libgl1`, `libgl1-mesa-dri` | `libglvnd-egl`, `mesa-dri-drivers` | `libglvnd`, `mesa` | eframe (glow + glutin) | Drawing the UI | **App cannot start** |
| `libwayland-client.so.0`, `libwayland-egl.so.1`, `libxkbcommon.so.0` | `libwayland-client0`, `libwayland-egl1`, `libxkbcommon0` | `libwayland-client`, `libxkbcommon` | `wayland`, `libxkbcommon` | winit (Wayland backend) | Window and input on Wayland | Falls back to X11 |
| `libX11`, `libxcb`, `libXcursor`, `libXrandr`, `libXi` | `libx11-6`, `libxcursor1`, `libxrandr2`, `libxi6` | `libX11`, … | `libx11`, … | winit (X11 backend) | Window and input on X11 | Needed when there is no Wayland session |
| D-Bus + `xdg-desktop-portal` (+ a GNOME, KDE or GTK backend) | `xdg-desktop-portal`, `xdg-desktop-portal-gnome` | same | same | `rfd` (pure-Rust zbus, no library) | Open, save and export file dialogs | File dialogs do not open |
| glibc | — | — | — | the binary | C runtime | Binaries only run on glibc at least as new as the build machine's |

**Plugin features Studio provides:** `urid:map/unmap`, `options`, `buf-size:boundedBlockLength`, `worker:schedule` (one worker thread per instance), `state:interface` save/restore, and for GUIs `ui:parent`, `instance-access`, `ui:idleInterface` and `ui:resize`. GUIs exchange atom messages with their plugin in both directions.

**Plugins** are found in `LV2_PATH`. If it is unset, `lv2::setup_lv2_path` sets `~/.lv2:/usr/local/lib/lv2:/usr/lib/lv2:/usr/lib64/lv2:/usr/lib/<multiarch>/lv2`. Plugin GUIs bring their own dependencies; for example, x42 GUIs use OpenGL through GLX, and JUCE GUIs use X11.

## Rust crates (direct dependencies)

| Crate | Version | Purpose | Portability notes |
|---|---|---|---|
| `eframe` / `egui` | 0.35 | UI framework (OpenGL via glow) | Linux, macOS, Windows, web. 0.35 needs Rust ≥ 1.92 (0.37 needs 1.95) |
| `egui-phosphor` | 0.13 | Icon font | Pure Rust |
| `jack` | 0.13 | JACK client, loads libjack at runtime | JACK exists on macOS and Windows too, but is uncommon there |
| `libloading` | 0.9 | `dlopen` for lilv, libX11, LAME, plugin GUIs, VST3 modules | Cross-platform (`LoadLibrary` on Windows) |
| `vst3` | 0.3 | VST3 interfaces, generated from the SDK headers (MIT since 2025); Studio implements the host side | Pure Rust bindings, no SDK needed at build time |
| `rtrb` | 0.4 | Lock-free ring buffers (UI ↔ audio thread) | Pure Rust |
| `arc-swap` | 1.9 | Lock-free arrangement snapshot for the audio thread | Pure Rust |
| `hound` | 3.5 | WAV read and write | Pure Rust |
| `rfd` | 0.17 | Native file dialogs (xdg portal on Linux) | Cross-platform |
| `serde`, `serde_json` | 1.0 | Project files (`.studio` is JSON) | Pure Rust |
| `base64` | 0.22 | Plugin state blobs in project files | Pure Rust |
| `egui_kittest` (dev) | 0.35 | Headless UI tests | Pure Rust |

Build: Rust 1.92 or newer (developed with 1.94).

## Tests and their requirements

| Command | Needs |
|---|---|
| `cargo test` | Nothing extra (no audio server, no display) |
| `cargo test engine_ -- --ignored` | A running JACK server, e.g. `jackd -n studiotest -d dummy &` with `JACK_DEFAULT_SERVER=studiotest` |
| `cargo test tone3000 -- --ignored --test-threads=1` | TONE3000 in `~/.lv2` and an X display |
| `cargo test x42_tuner -- --ignored` | x42-plugins (`tuna.lv2`), an X display, and EGL |
| `cargo test vst3_tone3000 -- --ignored` (+ `vst3_from_app`) | TONE3000 in `~/.vst3`; the editor tests need an X display |
| `VST3_PATH=<dir> cargo test vst3_gain -- --ignored` | A VST3 bundle named *Gain* in `<dir>`, e.g. the `vst3` crate's `examples/gain.rs` built as a `cdylib` (also works for the Windows target under Wine) |

## Linux-specific code (portability hotspots)

| Where | What | Notes |
|---|---|---|
| `main.rs` `use_pipewire_jack` | Re-exec with `LD_LIBRARY_PATH` (Unix `exec`) | Linux only (`cfg`) |
| `backend/jack_backend.rs` | JACK client; reads `/proc/self/maps` to detect PipeWire | Linux only |
| `backend/wasapi_backend.rs` | WASAPI through the `windows` crate | Windows only |
| `backend/cpal_backend.rs` | ASIO through cpal (`--features asio`) | Windows only |
| `backend/winmidi.rs` | WinMM MIDI input through midir | Windows only |
| `vst3/editor.rs` | Editor windows: X11 + `IRunLoop` (Linux), Win32 (Windows) | macOS would need an NSView host |
| `lv2.rs` | `liblilv-0.so.0`, default LV2 directories | Library name and paths differ per OS |
| `lv2ui.rs` | `libX11.so.6`, `XInitThreads`, `ui:X11UI` only | macOS: `ui:CocoaUI`; Windows: `ui:WindowsUI`. GTK and Qt GUIs are not supported |
| `export.rs` | `libmp3lame.so.0` | `libmp3lame.dylib` / `libmp3lame.dll` elsewhere |
| `project.rs` | Default recording directory `$HOME/Music/Studio` | Use the platform's music folder |

## Portability plan

**Other Linux distributions:** the code needs no changes, only the packages in the table above. For a distro-independent bundle:
* **Flatpak:** use the freedesktop runtime with the PipeWire socket, and ship lilv and LAME.
* **AppImage:** bundle lilv, serd, sord, sratom, zix and LAME. Keep libjack, libGL/EGL and libX11 from the host system: they must match the host's sound server and GPU driver.

**Windows:** see [WINDOWS.md](WINDOWS.md). The engine runs on a `Backend` trait (`src/backend/`):
* JACK on Linux;
* on Windows: WASAPI driven directly (shared, low-latency shared and exclusive mode; every capture device), or ASIO through cpal with `--features asio`;
* MIDI through midir.

Windows-only crates: `windows` (WASAPI, COM, Win32 editor windows), `midir`, and `cpal` (only with the `asio` feature); `jack` is Linux-only. Library names are per-OS: `lilv-0.dll`, `libmp3lame.dll`.

**macOS:** add a CoreAudio backend through cpal (the `Backend` trait and `devio` layer are ready for it), set the `.dylib` names, and use CLAP/VST3/AU plugin hosting with Cocoa plugin GUIs.
