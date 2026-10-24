# Studio on Windows

Status: **working port** on the `windows-port` branch. The Windows build compiles, links, and passes the test suite under Wine. Under Wine, audio plays through WASAPI and audio input works. It has not yet been run on real Windows 11 hardware.

## Audio driver: decisions

| Decision | Choice | Why |
|---|---|---|
| Driver abstraction | Studio's own `Backend` trait (`src/backend/`). JACK stays native on Linux; cpal on Windows | JACK's model (per-track ports, connections to other apps) is worth keeping on Linux. On Windows a sound card exposes N input / M output channels, which is a different model. Both share one real-time mixer (`Processor`). |
| Windows audio library | [cpal](https://github.com/RustAudio/cpal) 0.18 | The standard Rust audio I/O crate. Covers WASAPI and ASIO, maintained, pure Rust apart from ASIO. |
| Default API | **WASAPI shared mode** | Works on every Windows 11 PC with no drivers or setup. Latency is the Windows mixer period, typically 10 ms (480 frames at 48 kHz). |
| Low-latency API | **ASIO** behind the `asio` cargo feature | The pro-audio standard on Windows: interface vendors ship ASIO drivers that bypass the Windows audio stack (a few ms at 64–128 frames). Without a vendor driver, use [FlexASIO](https://github.com/dechamps/FlexASIO) or ASIO4ALL. With ASIO built in, Studio picks it automatically when a driver is installed (`STUDIO_AUDIO=wasapi` forces WASAPI). |
| WASAPI exclusive mode | **Not yet** | Not in a released cpal (planned for cpal 0.19). Revisit when it ships; it gives near-ASIO latency on devices without an ASIO driver. |
| Audio thread priority | cpal `realtime` feature | Puts the audio thread in MMCSS ("Pro Audio"), with a fallback to `THREAD_PRIORITY_TIME_CRITICAL`. |
| MIDI | [midir](https://github.com/Boddlnagg/midir) (WinMM) | Windows 11 routes WinMM through the new **Windows MIDI Services**, which adds multi-client access, so keyboards can be shared with other apps. MIDI 2.0 via the MIDI Services App SDK is possible later. |
| Plugins | LV2 off by default on Windows | LV2 is rare on Windows, and Studio's plugin GUIs are X11-only. The code is portable: if `lilv-0.dll` and LV2 bundles are present (`%APPDATA%\LV2`, `%COMMONPROGRAMFILES%\LV2`), plugins load, without GUIs. The Windows-native plan is **CLAP/VST3** as new `Device` implementations (the VST3 SDK is MIT-licensed since 2025). |
| Licensing of ASIO | Opt-in feature | Steinberg's ASIO SDK is dual-licensed (proprietary or GPLv3) since Oct 2025. Studio has no license yet. Decide before distributing ASIO builds: GPLv3 for Studio, or sign Steinberg's free proprietary agreement. |

How the Windows backend works (`src/backend/cpal_backend.rs` + `devio.rs`):
* The **output** stream's callback runs the mixer.
* The **input** stream's callback pushes captured audio into a lock-free ring. The mixer drains it each cycle and drops anything beyond 4 periods, so drift between input and output clocks can't build up latency. With ASIO both streams come from the same driver callback cycle.
* **Tracks** pick input channels (`In 1`, `In 2`, …) and a MIDI port (or all ports). The master plays on the output device's first two channels. ASIO and WASAPI streams in `i16`, `i32` or `f32` are converted to and from the `f32` mixer.
* **Buffer size** changes rebuild the streams. The picker offers the powers of two the device accepts, plus its own minimum and maximum. Under ASIO the driver's control panel may also limit sizes.
* **Output device** switching rebuilds the streams on the chosen device. Input is the default capture device under WASAPI, or the same driver under ASIO.

## Building

### On Windows (native)

1. Install Rust with the MSVC toolchain (`rustup default stable-x86_64-pc-windows-msvc`) and the "Desktop development with C++" workload of Visual Studio Build Tools.
2. `cargo build --release`. The result is `target\release\studio.exe`, a single executable.
3. Optional MP3 export: put `libmp3lame.dll` (x64) next to `studio.exe`.

With ASIO:
1. Install LLVM and set `LIBCLANG_PATH` (e.g. `C:\Program Files\LLVM\bin`); cpal's ASIO bindings are generated with bindgen.
2. `cargo build --release --features asio`. cpal downloads the ASIO SDK automatically, or uses `CPAL_ASIO_DIR` if set.
3. Install your interface's ASIO driver, or FlexASIO.

### From Linux (cross-compile and test with Wine)

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt install mingw-w64 wine                # linker and test runner
cargo build --target x86_64-pc-windows-gnu
WINEDEBUG=-all cargo test --target x86_64-pc-windows-gnu
# real audio through Wine's WASAPI (winepulse -> PipeWire/PulseAudio):
WINEDEBUG=-all cargo test --target x86_64-pc-windows-gnu windows_backend -- --ignored --nocapture
WINEDEBUG=-all wine target/x86_64-pc-windows-gnu/debug/studio.exe
```

`.cargo/config.toml` sets the mingw-w64 linker and makes `wine` the runner for this target. The ASIO feature cannot be cross-built this way, because it needs MSVC and the SDK.

## Verified (under Wine 10 on Linux)

* The whole test suite passes as a Windows binary (UI tests, export to WAV and MP3, editors, snippets, device I/O routing).
* `windows_backend_plays_the_synth`: the WASAPI backend opens at 48 kHz with a 480-frame period. The built-in synth plays a MIDI clip to the master, DSP load is about 3%, and output devices are listed.
* `studio.exe` starts and shows the UI. The status bar shows "WASAPI · <device>" and the period, and an audio track meters live input.

## Not done yet

* Testing on real Windows 11 hardware and with ASIO drivers (including measuring round-trip latency).
* WASAPI exclusive mode (waiting for cpal 0.19).
* Choosing the input device separately from the output under WASAPI (it uses the default capture device).
* CLAP/VST3 plugin hosting and native plugin GUIs.
* An installer (Studio ships as a single `.exe` for now) and code signing.
