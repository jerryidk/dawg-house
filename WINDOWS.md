# Studio on Windows

Status: **working port**, released as an NSIS installer (`scripts/release.sh`). The Windows build compiles and links, and its test suite passes under Wine. Under Wine, WASAPI plays and records (shared mode down to 256 frames), and VST3 plugins load, process and open editor windows. It has not yet been run on real Windows 11 hardware.

## Audio driver: decisions

| Decision | Choice | Why |
|---|---|---|
| Driver abstraction | Studio's own `Backend` trait (`src/backend/`). JACK stays native on Linux | JACK's model (per-track ports, connections to other apps) is worth keeping on Linux. On Windows a sound card exposes N input / M output channels, which is a different model. Both share one real-time mixer (`Processor`). |
| WASAPI implementation | **Studio drives WASAPI itself** (`wasapi_backend.rs`, `windows` crate) instead of through cpal | cpal's WASAPI host always runs at the Windows mixer period (10 ms; a smaller buffer only adds ring latency), opens only the default capture device, and only at that device's native rate. Each limit was visible in the app: one input device, no latency below 10 ms. |
| Default mode | **Shared, low latency** through `IAudioClient3` | Works on every Windows 10/11 PC with no drivers, alongside other apps. When Studio runs at the Windows mixer's rate, `InitializeSharedAudioStream` gives periods down to the driver's minimum (typically 128 frames ≈ 2.7 ms at 48 kHz with the inbox HD Audio driver; the picker shows what the driver allows). If the driver refuses, Studio falls back to the standard 10 ms period. |
| Other rates | Windows resampler (`AUTOCONVERTPCM`) in shared mode | Any rate works, at the standard 10 ms period (the low-latency path requires the mixer's own format). |
| **Exclusive mode** | Selectable in the status bar | Studio owns the output device and bypasses the Windows mixer: any rate and period the driver supports (often 3 ms or less). Other apps go silent on that device. Studio tries 32-bit float, then 32/24-bit and 16-bit integer formats. Input devices are opened exclusively too, falling back to shared mode with resampling. |
| Inputs | **Every active capture device** is a source (`<device> · In N`) | A device's capture stream opens only while a track uses one of its channels. The Windows default recording device is listed first, so new tracks use it. A track's left and right can come from different devices. Each device has its own lock-free ring; the mixer bounds each one to 4 periods, so drift between device clocks cannot build up latency. |
| Low-latency API | **ASIO** behind the `asio` cargo feature (through cpal) | The pro-audio standard on Windows: interface vendors ship ASIO drivers that bypass the Windows audio stack. With ASIO built in, Studio picks it when a driver is installed (`STUDIO_AUDIO=wasapi` forces WASAPI). |
| Audio thread priority | MMCSS "Pro Audio" (`AvSetMmThreadCharacteristicsW`) | For every WASAPI stream thread; event-driven (`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`). |
| Settings | Remembered per machine in `%APPDATA%\Studio\audio.json` | Mode, rate, period and output device. If they no longer work (device unplugged), Studio starts with defaults. |
| Sample rate changes | Rebuild the session | Positions are stored in samples. The project now records its rate; on a rate change (or when opening a project made at another rate) positions are converted, audio is reloaded at the new rate, and plugins are re-created with their state. |
| MIDI | [midir](https://github.com/Boddlnagg/midir) (WinMM) | Windows 11 routes WinMM through the new **Windows MIDI Services**, which adds multi-client access, so keyboards can be shared with other apps. MIDI 2.0 via the MIDI Services App SDK is possible later. |
| Plugins | **VST3** (Linux and Windows); LV2 off by default on Windows | VST3 is the plugin format on Windows. Hosting uses the `vst3` crate's generated bindings (the SDK is MIT-licensed since 2025); editors open in a native window. LV2 is rare on Windows and Studio's LV2 GUIs are X11-only. If `lilv-0.dll` and LV2 bundles are present (`%APPDATA%\LV2`, `%COMMONPROGRAMFILES%\LV2`), LV2 plugins load, without GUIs. |
| Licensing of ASIO | Opt-in feature | Steinberg's ASIO SDK is dual-licensed (proprietary or GPLv3) since Oct 2025, and Studio is MIT. Decide before distributing ASIO builds: GPLv3 for those builds, or sign Steinberg's free proprietary agreement. |

How the WASAPI backend works (`src/backend/wasapi_backend.rs` + `devio.rs`):
* The **output** stream's thread runs the mixer each time WASAPI signals its event.
* Each **input** device's stream thread pushes captured audio into its own ring buffer; the mixer reads each one per cycle.
* The **status bar** has three pickers: mode (Shared / Exclusive), sample rate, and period (the sizes this device and mode accept, with their latency). A refused setting is reported and the previous one stays.
* **Output device** switching reopens the streams on the chosen device, starting from its default period.

## VST3 hosting

* **Threads:** a dedicated plugin thread loads modules, creates and destroys instances, saves state, and hosts editors (on Linux it also runs the `IRunLoop` that JUCE and VSTGUI editors need). The audio thread only calls `IAudioProcessor::process`. JUCE plugins require one consistent message thread, which egui's thread cannot be.
* **Parameters:** the generic sliders and the plugin's editor share the device's parameters; the audio thread sends changes to the processor, and the plugin thread keeps the controller (and so the editor) in sync. MIDI CC, aftertouch and pitch bend go through the plugin's `IMidiMapping`; the hundreds of MIDI-CC proxy parameters JUCE adds are hidden.
* **State:** component and controller state are saved in the project, base64 like LV2 state.
* **Class ids** are stored in the SDK's canonical form (`vst3:ABCDEF01…`), which is the same on Linux and Windows, so projects move between systems.

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
sudo apt install mingw-w64 wine nsis           # linker, test runner, installer
cargo build --target x86_64-pc-windows-gnu
WINEDEBUG=-all cargo test --target x86_64-pc-windows-gnu
# WASAPI end to end through Wine (winepulse -> PipeWire/PulseAudio):
WINEDEBUG=-all cargo test --target x86_64-pc-windows-gnu wasapi_ -- --ignored --nocapture
# VST3 with a Windows build of the vst3 crate's example gain plugin:
VST3_PATH='Z:\path\to\dir' WINEDEBUG=-all cargo test --target x86_64-pc-windows-gnu -- --ignored vst3_gain editor_window
WINEDEBUG=-all wine target/x86_64-pc-windows-gnu/debug/studio.exe
```

`.cargo/config.toml` sets the mingw-w64 linker and makes `wine` the runner for this target. The ASIO feature cannot be cross-built this way, because it needs MSVC and the SDK.

## Verified (under Wine 10 on Linux)

* The whole test suite passes as a Windows binary (UI tests, export to WAV and MP3, editors, snippets, device I/O routing with several input devices).
* `wasapi_modes_rates_and_inputs`: all five capture devices of the test machine are listed and one opens on demand; shared low-latency mode runs at 256 frames (5.3 ms; Wine's minimum, down from 480); a switch to 44.1 kHz reopens through the resampler; exclusive mode is refused by Wine and Studio keeps running.
* `windows_backend_plays_the_synth`: the built-in synth plays a MIDI clip to the master.
* `vst3_gain_plugin_scales_audio` and `editor_window_attach_resize_close`: a Windows VST3 module loads, processes, takes parameter changes and bypass, and an editor window attaches, resizes and closes.
* `studio.exe` starts and shows the UI with the mode, rate and period pickers; a new audio track listens to the default recording device.

## Not done yet

* Testing on real Windows 11 hardware: exclusive mode and the low-latency shared periods of real drivers, ASIO drivers, round-trip latency, and real VST3 plugins with their editors.
* Choosing the input device's rate separately (inputs run at the session rate, resampled by Windows in shared mode).
* CLAP plugin hosting.
* Code signing (the installer and `studio.exe` are unsigned, so SmartScreen will warn).
