//! MIDI input on Windows through midir (WinMM, which Windows 11 routes
//! through Windows MIDI Services). Shared by the WASAPI and ASIO backends.

use super::devio::MidiInEvent;
use std::sync::{Arc, Mutex};

/// Open connections to every MIDI input port; messages go to `tx` tagged
/// with the port's index in `names`.
pub struct MidiInputs {
    pub names: Vec<String>,
    _connections: Vec<midir::MidiInputConnection<()>>,
}

impl MidiInputs {
    pub fn open(tx: rtrb::Producer<MidiInEvent>) -> Self {
        let mut inputs = MidiInputs { names: vec![], _connections: vec![] };
        let tx = Arc::new(Mutex::new(tx));
        let Ok(probe) = midir::MidiInput::new("Studio") else { return inputs };
        let count = probe.ports().len();
        for i in 0..count {
            // `connect` consumes the MidiInput, so use one per port.
            let Ok(input) = midir::MidiInput::new("Studio") else { continue };
            let ports = input.ports();
            let Some(port) = ports.get(i) else { continue };
            let name = input.port_name(port).unwrap_or_else(|_| format!("MIDI {}", i + 1));
            let tx = tx.clone();
            let source = inputs.names.len() as i32;
            let conn = input.connect(
                port,
                "studio-in",
                move |_, msg, _| {
                    if let Some(e) = MidiInEvent::new(source, msg)
                        && let Ok(mut t) = tx.lock()
                    {
                        let _ = t.push(e);
                    }
                },
                (),
            );
            match conn {
                Ok(c) => {
                    inputs._connections.push(c);
                    inputs.names.push(name);
                }
                Err(e) => eprintln!("studio: MIDI input {name}: {e}"),
            }
        }
        inputs
    }

    /// `TrackShared::midi_source` value for a source name.
    pub fn source_index(&self, source: Option<&str>) -> i32 {
        match source {
            Some(super::ALL_SOURCES) => -2,
            Some(name) => self.names.iter().position(|n| n == name).map_or(-1, |i| i as i32),
            None => -1,
        }
    }
}
