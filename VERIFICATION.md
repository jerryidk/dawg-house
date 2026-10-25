# Formal verification with Kani

Studio uses the [Kani](https://github.com/model-checking/kani) model checker on the code at its **unsafe and external boundaries**: the places where bytes or values come from plugins, plugin GUIs, worker threads or audio drivers. A Kani proof checks a function for **every** input within the stated bounds: no panics, no out-of-bounds access, no arithmetic overflow, no undefined behaviour in `unsafe` code, and the assertions hold.

This is a DAW, not safety-critical software, so the approach is deliberately pragmatic. Each proof must finish in about **30 seconds** within a **4 GB** memory cap. A proof that doesn't fit gets smaller bounds or is dropped; it does not get a bigger budget.

## Running

```sh
cargo install --locked kani-verifier && cargo kani setup   # once
scripts/kani.sh                    # all proofs, one at a time
scripts/kani.sh atom_ frames_      # proofs whose name contains a pattern
```

`scripts/kani.sh` runs each proof in its own memory-capped scope (systemd user scope with `MemoryHigh` at 3/4 of the cap, `MemoryMax`, and no swap; `prlimit` as a fallback) under a timeout. It compiles with two parallel jobs and writes logs and `summary.txt` to `target/kani-logs/`. Settings: `KANI_MEM` (default `4G`), `KANI_TIMEOUT` (seconds, default `30`), `CARGO_BUILD_JOBS` (default `2`).

The proofs live in `src/verification.rs` and are only compiled under `cfg(kani)`. They don't affect normal builds.

## What is proved

| Proof | Boundary | Property |
|---|---|---|
| `atom_byte_views_are_exact` | `atom::as_bytes{,_mut}`: `from_raw_parts` over plugin port buffers | Views cover exactly the words, are in bounds, and map bytes correctly |
| `atom_parse_any_plugin_output` | `atom::for_each_event`: sequences **written by plugins** | Any bytes, any sizes: stays in bounds and yields only whole atom headers |
| `atom_append_any_header` | `atom::seq_append` (header may have been overwritten by a plugin) | Never writes out of bounds; grows by exactly one padded event or not at all |
| `atom_roundtrip` | Studio → plugin input sequences | Events written are read back unchanged |
| `frames_pop_garbage` | `frames::pop_with` over GUI / worker queues | Arbitrary queued bytes (e.g. a corrupt length) never panic or over-read |
| `frames_reject_oversized` | `frames::push` (`write_chunk_uninit` + `commit_all`) | A message larger than the free space is rejected whole; nothing is committed |
| `gui_control_value_any_pointer` | `lv2ui::decode_control`: pointer **from a plugin GUI** | Any address and size: no UB, value read only when present |
| `state_retrieve_any_key` | `lv2::retrieve_cb`: called **by plugins** during state restore | Returns exactly the stored value; null out-pointers are tolerated |
| `device_input_routing_any_channel` | `devio::DeviceIo::track_audio`: channel indices from atomics | Any `i32` channel id (device × 64 + channel, several input devices) selects valid buffers of the cycle's length, or none |
| `vst3_stream_any_seek_read_write` | `vst3::com::MemStream` (`IBStream` handed to plugins for state) | Any seek mode, seeks near the data, any read/write size stay in bounds; growth stops at the write's end (huge seeks are refused, unit-tested) |
| `vst3_param_changes_any_use` | `vst3::com::ParamChanges` (`IParameterChanges` handed to processors) | Any ids, indices and points stay inside the preallocated queue pool |
| `device_deinterleave_any_channel_count` | `devio::deinterleave`: channel count **from the driver** | No out-of-bounds access for 0–2 channels and any wrap split |
| `audio_sample_any_shape` | `AudioData::sample` (audio thread) | Never panics for any channel count or index |
| `midi_messages_any_length` | `MidiInEvent::new`, `MidiEvent::new`: bytes **from MIDI drivers** | No panic for any length; only 1–3 byte messages are accepted |
| `peak_scan_any_range` | `audio::range_peak` | Any indices are safe |
| `render_clip_any_placement` | `engine::render_clips` index math (audio thread) | Any clip start, offset, length, fades and position: no overflow or out-of-bounds access |
| `loop_block_stays_in_loop` | `engine::loop_block` | Playback never leaves the loop once inside it, and the split stays inside the block (±2^40 samples) |

## Bugs Kani found (fixed)

1. **Undefined behaviour in the plugin-GUI callback** (`lv2ui::decode_control`): the control value was read through the GUI's pointer with `*(ptr as *const f32)`, and nothing guarantees that pointer is aligned. Fixed with `ptr::read_unaligned`.
2. **Panic on the audio thread** (`AudioData::sample`): `channels.len() - 1` underflowed for audio with no channels. The current loaders never create such data, so it was not reachable yet; it is fixed defensively because a panic there aborts the audio callback.

## Not covered

* Queue **round-trips** through the real `rtrb` ring (including messages wrapping the ring end) don't fit the 30 s budget and were dropped. The regular test suite exercises these paths: the plugin GUI, worker and drum-kit tests.
* Code that needs real libraries or threads (lilv, libX11, JACK, cpal, plugin instances) can't be model-checked; the integration tests cover it.
* Proofs are bounded (small buffers and sizes). They show the absence of errors up to those bounds, not for every possible size.
