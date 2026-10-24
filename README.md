# Studio

A recording studio / DAW for Linux built around **recording by pieces**. Each track (one instrument layer: guitar, bass, drums, vocals…) owns **snippets**: short pieces with their own tempo, meter and length in bars. You record into a snippet as many times as you like, and every loop pass becomes a **take**. Then you assemble the song on a linear timeline from **linked placements** of those snippets. To fix a mistake, re-record one snippet and every place it's used updates.

Studio also has non-destructive editing, audio and instrument (MIDI) tracks, and LV2 plugins. It runs on PipeWire through its JACK emulation.

## Build and run

See [DEPENDENCIES.md](DEPENDENCIES.md) for every runtime library, the sound stack and notes on portability. Windows: see [WINDOWS.md](WINDOWS.md).


```sh
sudo apt install pipewire-jack   # PipeWire's libjack (recommended)
cargo run --release
```

No `-dev` packages are needed. libjack and liblilv are loaded at runtime. `build-support/pkgconfig/jack.pc` is a build-time stub that satisfies `jack-sys`.

| Option | Effect |
|---|---|
| `--latency N` | Request a PipeWire quantum of N frames (`PIPEWIRE_LATENCY=N/48000`) |
| `--jackd` | Use the system libjack (a real `jackd`) instead of PipeWire |
| `PROJECT.studio` | Open a project |

**PipeWire:** on start-up Studio behaves like `pw-jack`. If PipeWire's `libjack.so.0` is installed (`/usr/lib/*/pipewire-0.3/jack`), the app re-executes itself with that directory first in `LD_LIBRARY_PATH`. The status bar shows **PipeWire (JACK)** when that worked. Ports show up in `qpwgraph`/Helvum as `studio:*`.

## Using it

The window has three foldable panels: **Snippets** (one column per track), **Timeline** (the song) and **Detail** (Takes, Editor, Devices). Click a panel's ▾/▸ header to fold or expand it.

* **Snippets:** "+ New snippet" in a track's column. Set its name, tempo, meter (e.g. 6/8) and length in bars in the **Takes** tab. Click a card to loop it. The other tracks then play their *cued* snippet (the one last selected in each column) as backing.
* **Recording takes:** press ⏺ on a card (or select it and press **R**). After a one-bar count-in (toggle ⏱ in the transport) the snippet loops, and every full pass becomes a new take. The take being replaced is muted while you record. Pick the active take in the **Takes** tab. Instrument snippets can also start from an empty MIDI take drawn in the piano roll.
* **Assembling:** drag a snippet card onto its track's lane in the Timeline (or use "Place at playhead"). Placements are **linked**: changing the active take or editing notes updates every placement. Stretch a placement past its end to repeat the loop. Right-click a placement and choose **Make unique** to fork an independent copy. Click the ruler or a lane to go back to playing the song (or use the **Song** button).
* **Tempo:** each snippet keeps its own tempo and meter. Snippets are not time-stretched, so a 96 BPM snippet stays 96 BPM wherever it's placed. "Song BPM" sets the timeline grid and the default tempo for new snippets. The metronome follows whatever is playing.

* **Tracks:** `+ Audio` / `+ Instrument` (Ctrl+T / Ctrl+Shift+T). Each header has arm ●, monitor 🎧, mute, solo, volume, pan, an input selector, and an input meter (pre-fader) plus an output meter (post-fader). Right-click a header to change its colour or delete the track.
* **Monitoring:** for audio tracks, 🎧 sends the live input through the track's devices to the output. For instrument tracks it plays the instrument live from MIDI input, and arming does the same.
* **Recording on the timeline:** with the song playing, arm tracks and press **R** to record free clips at the playhead. Audio is written as 32-bit float WAV to `~/Music/Studio/session-*/audio/`. MIDI becomes a MIDI clip.
* **Editing:** drag a clip to move it (also between tracks of the same kind), drag its edges to trim, and drag the top handles of audio clips for fades. **S** splits at the playhead, **Ctrl+D** duplicates, **Del** deletes, **Ctrl+Z / Ctrl+Shift+Z** undo and redo. Snap follows the visible grid; hold **Alt** to bypass it.
* **Piano roll:** double-click an instrument lane to create a MIDI clip. In the **Editor** tab, click to add a note, drag to move, drag the right edge to resize, and right-click to delete. Drag in the velocity lane to set velocity. **Q** quantizes.
* **Instruments:** instrument tracks play MIDI into their first device: the built-in synth, or an LV2 instrument such as the AVL drum kits (e.g. *Black Pearl Drumkit*). **Change instrument** in the Devices tab swaps it. Drum kits load their samples in the background (LV2 worker extension). *Multi* kits are mixed down to stereo using the plugin's own left/right port markings.
* **Devices:** in the **Devices** tab, add LV2 or built-in devices, bypass, reorder and remove them, and edit their parameters. Plugins that ship their own interface (e.g. TONE3000) have an **Open GUI** button, which opens it in a separate window (X11; XWayland on Wayland desktops). A plugin's internal state, such as a loaded amp model, is saved in the project.
* **Editor tab:** timeline clips and snippet takes use the same editors. MIDI opens in the piano roll and audio in the **waveform editor**. The editor shows the selected clip, the selected snippet's active take, or a take opened with **Edit** in the Takes tab. The waveform editor draws the audio against the region's own bars and beats (the snippet's tempo and meter for takes). Zoom with Ctrl+wheel, −/+ or Fit. Drag the audio to slip it against the grid (snapped; hold Alt for free movement), drag the edges to trim timeline clips, drag the top corners for fades, and set gain. A take's gain and fades apply on every loop pass.
* **Navigation:** Space play/pause, Home to the start, Ctrl+wheel zoom, Shift+wheel scroll. Click the ruler to move the playhead.
* **Files:** save and open projects (`.studio`, JSON) and import WAV files (Ctrl+I, or drop them on a lane).
* **Export:** **File → Export audio…** (Ctrl+E) renders the timeline offline, faster than real time and with every device and plugin, to WAV (16-bit, 24-bit or 32-bit float) or MP3 (320/192 kbps, via the system's `libmp3lame0`). A tail setting lets reverbs and note releases ring out after the last clip.
* **Audio setup:** the picker next to the master volume chooses the output device (saved per project). The status bar's frames picker sets the period size (32–4096 frames) and shows the latency of each choice. Under PipeWire this forces the graph quantum for all apps.

## Architecture

```
src/
  main.rs       entry point, pw-jack re-exec, CLI
  device.rs     Device trait, DeviceInfo/ParamInfo, DeviceRegistry   ← extension point
  builtin.rs    built-in devices (Basic Synth, Utility)
  lv2.rs        LV2 host on liblilv (dlopen): discovery, URID map, options, worker, MIDI→atom, state, multi-out downmix
  lv2ui.rs      plugin GUI windows (X11UI via dlopen'd libX11, instance-access, idle/resize)
  engine.rs     JACK real-time engine: tracks, device chains, clip playback, recording
  audio.rs      audio sources, WAV import/resampling, disk-writer for recording
  project.rs    model: TrackKind, Snippet, Take, Clip, ClipContent, Note, DeviceSlot, undo history
  song.rs       pure song logic: linked placements → playback list, snippet loops, loop takes
  export.rs     offline render to WAV / MP3 (libmp3lame via dlopen)
  app.rs        app state, transport, device panel/browser, project I/O
  snippets.rs   snippet columns and the Takes tab
  timeline.rs   linear timeline (headers, lanes, placements, non-destructive clip editing)
  pianoroll.rs  MIDI editor
  waveedit.rs   waveform editor for audio clips and takes
  theme.rs      visual style and small widgets
```

* **Device trait** (`device.rs`): everything in a track's chain implements `Device::process(&mut ProcessContext)`, which gets stereo audio in place plus sorted MIDI events. LV2 plugins and built-ins use the same trait, parameter model (`DeviceShared` atomics) and generic UI. To add a new kind (for example CLAP), implement `Device` and give it an id prefix in `DeviceRegistry::create`.
* **Track kinds** (`project::TrackKind`): `Audio` (JACK audio inputs, recording to WAV) and `Instrument` (JACK MIDI input, MIDI recording; the first device consumes the notes).
* **Snippets and takes** (`project::Snippet`, `Take`): a snippet has its own tempo, meter and length in bars, plus takes with one active. A loop recording is a single WAV file, and each take is a window onto one pass of it.
* **Clip content** (`project::ClipContent`): `Audio { source }`, `Midi { notes }` or `Snippet { snippet }` (a linked placement, which loops the snippet's active take). Clips are windows (`start`, `offset`, `length`) onto their content, so editing never changes the content.
* **Playback** (`song.rs`): the project is compiled into a flat `Arrangement` of playback clips, either for the song or for one snippet looping with cued backing. The engine plays that immutable snapshot (published with `arc-swap`). It handles the loop range, the count-in (negative positions) and the metronome, and starts capturing only at the loop start.
* **Real-time safety:** the UI sends commands over a lock-free `rtrb` queue. Objects removed on the audio thread (tracks, devices, ring buffers) go back over a second queue so they are freed on the UI thread. The audio callback does not allocate or lock.

## UI toolkit choice

The UI uses [egui](https://github.com/emilk/egui) (via `eframe`, OpenGL; Wayland and X11). Among the modern Linux-compatible toolkits I looked at (GTK4/libadwaita, Qt 6, Slint, iced, egui), egui fits a DAW well. It is immediate-mode, so meters, waveforms and the playhead redraw every frame without widget-state plumbing, and custom painting of timelines and piano rolls is simple. It needs no system development packages, and its look can be fully themed. iced was the closest alternative.

## Limits / next steps

* LV2: only X11 plugin GUIs are hosted (no GTK/Qt GUIs). MIDI-only plugins (MIDI in, MIDI out, no audio) are not hosted. Multi-output instruments are downmixed to the track's stereo (no per-output routing to separate tracks yet).
* There is no automation or time-stretching yet. Export covers the whole timeline (no range selection). Recording has no latency compensation, so takes may sit a few milliseconds late against the grid. Comping (combining bars from different takes) is not implemented.
* Import is WAV only.

## Tests

```sh
cargo test                                    # unit + headless UI tests (egui_kittest)
jackd -n studiotest -d dummy &                # optional: end-to-end through JACK
JACK_DEFAULT_SERVER=studiotest cargo test engine_ -- --ignored
cargo test tone3000 -- --ignored --test-threads=1   # opens TONE3000's GUI (needs the plugin + a display)
```
