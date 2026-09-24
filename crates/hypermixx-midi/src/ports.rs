//! MIDI port enumeration and input connection.
//!
//! Thin on purpose: `midir` owns the ALSA/CoreMIDI plumbing, [`crate::msg::Decoder`] owns the
//! bytes, and this module only turns a port query into an open connection whose parsed events flow
//! down a channel. The connection is kept alive by the value returned from [`open`]; dropping it
//! (or the owning front-end) closes the port, which is what lets a run-mode front-end stop the
//! input before it sends `Quit`.

use std::time::SystemTime;

use crossbeam_channel::Sender;
use midir::{Ignore, MidiInput, MidiInputConnection};

use crate::msg::{Decoder, Event};

/// A parsed event and the wall-clock time it arrived, for the guide's raw monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    pub at: SystemTime,
    pub event: Event,
}

/// A MIDI input port as shown to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortInfo {
    pub index: usize,
    pub name: String,
}

/// Every available MIDI input port, in the order the backend reports them.
pub fn list_ports() -> Result<Vec<PortInfo>, String> {
    let input = MidiInput::new("hypermixx").map_err(|err| err.to_string())?;
    let ports = input.ports();
    Ok(ports
        .iter()
        .enumerate()
        .map(|(index, port)| PortInfo {
            index,
            name: input
                .port_name(port)
                .unwrap_or_else(|_| format!("port {index}")),
        })
        .collect())
}

/// An open MIDI input connection.
///
/// Opaque on purpose: the owning front-end only needs to keep it alive and drop it to stop the
/// callback thread, so `midir` stays an implementation detail of this crate.
pub struct Input {
    _connection: MidiInputConnection<Decoder>,
}

/// Opens the input port named `query` and forwards parsed events to `tx` for as long as the
/// returned connection lives.
///
/// `query` is either a 0-based index (as [`list_ports`] reports) or an exact port name. A bad query
/// is a clean error listing the alternatives, matching the CLI's habit of failing a bad `--config`
/// instead of starting half-configured.
pub fn open(query: &str, tx: Sender<Received>) -> Result<Input, String> {
    let mut input = MidiInput::new("hypermixx").map_err(|err| err.to_string())?;
    // The decoder already drops clock/active-sensing/sysex; keep the backend transparent so there
    // is one place that decides what an event is.
    input.ignore(Ignore::None);
    let ports = input.ports();
    if ports.is_empty() {
        return Err("no MIDI input ports available".to_owned());
    }
    let names: Vec<String> = ports
        .iter()
        .map(|port| {
            input
                .port_name(port)
                .unwrap_or_else(|_| "unknown port".to_owned())
        })
        .collect();
    let index = select_index(&names, query)?;
    let port = ports[index].clone();
    let connection = input
        .connect(
            &port,
            "hypermixx-in",
            move |_timestamp, message, decoder: &mut Decoder| {
                let at = SystemTime::now();
                for event in decoder.feed(message) {
                    // A disconnected receiver means the front-end is shutting down; stop quietly.
                    let _ = tx.send(Received { at, event });
                }
            },
            Decoder::new(),
        )
        .map_err(|err| err.to_string())?;
    Ok(Input { _connection: connection })
}

/// Resolves a port query (index or exact name) against the reported names.
fn select_index(names: &[String], query: &str) -> Result<usize, String> {
    if let Ok(index) = query.parse::<usize>() {
        return if index < names.len() {
            Ok(index)
        } else {
            Err(format!(
                "MIDI port index {index} is out of range (0..{})",
                names.len()
            ))
        };
    }
    names
        .iter()
        .position(|name| name == query)
        .ok_or_else(|| {
            format!(
                "no MIDI input port named `{query}` (available: {})",
                names.join(", ")
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["Launch Control".to_owned(), "Virtual Raw MIDI 1-0".to_owned()]
    }

    #[test]
    fn a_port_can_be_selected_by_index_or_exact_name() {
        assert_eq!(select_index(&names(), "0").unwrap(), 0);
        assert_eq!(select_index(&names(), "1").unwrap(), 1);
        assert_eq!(select_index(&names(), "Virtual Raw MIDI 1-0").unwrap(), 1);
    }

    #[test]
    fn unknown_ports_report_the_alternatives() {
        let err = select_index(&names(), "7").unwrap_err();
        assert!(err.contains("out of range"), "{err}");
        let err = select_index(&names(), "Nope").unwrap_err();
        assert!(err.contains("Launch Control"), "{err}");
    }
}
