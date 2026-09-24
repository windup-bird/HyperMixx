//! MIDI byte stream → typed [`Event`].
//!
//! Deliberately hand-written: v1 only needs NoteOn/Off, ControlChange and PitchBend, plus the
//! filtering every mapping layer must do (drop clock/active-sensing, follow running status, swallow
//! sysex). A crate for that would be more surface than the ~150 lines below, and `midi-msg`'s
//! edition-2024 requirement would break the workspace's 1.70+ promise.
//!
//! The parser is a state machine over *bytes*, not a whole-message function, because a driver may
//! hand the callback a split message or a running-status run. Accumulate through [`Decoder::feed`].

/// One inbound MIDI event the mapping layer understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    NoteOn {
        channel: u8,
        key: u8,
        velocity: u8,
    },
    NoteOff {
        channel: u8,
        key: u8,
        velocity: u8,
    },
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
    /// 14-bit bend in `-8192..=8191`, centred at 0.
    PitchBend {
        channel: u8,
        value: i16,
    },
}

impl Event {
    /// The MIDI channel (0-based).
    pub fn channel(&self) -> u8 {
        match *self {
            Event::NoteOn { channel, .. }
            | Event::NoteOff { channel, .. }
            | Event::ControlChange { channel, .. }
            | Event::PitchBend { channel, .. } => channel,
        }
    }

    /// A short label for the event monitor: `CC`, `NOTE`, `BEND`.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Event::NoteOn { .. } | Event::NoteOff { .. } => "NOTE",
            Event::ControlChange { .. } => "CC",
            Event::PitchBend { .. } => "BEND",
        }
    }
}

/// A streaming MIDI parser. Feed it every byte the input port delivers; it yields whatever
/// complete events those bytes finished.
#[derive(Debug, Default)]
pub struct Decoder {
    /// The status byte most recently seen, for running status.
    status: Option<u8>,
    data: [u8; 2],
    len: usize,
    /// Inside a system-exclusive dump: skip until the terminator.
    sysex: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes `bytes`, returning every complete event they produced.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        for &byte in bytes {
            self.push(byte, &mut events);
        }
        events
    }

    fn push(&mut self, byte: u8, out: &mut Vec<Event>) {
        // System real-time (0xF8..=0xFF) may interleave anywhere, even inside another message.
        // Clock, start/stop/continue and active sensing all land here and are simply dropped.
        if byte >= 0xF8 {
            return;
        }
        if self.sysex {
            if byte == 0xF7 {
                self.sysex = false;
            }
            return;
        }
        if byte & 0x80 != 0 {
            self.len = 0;
            if byte == 0xF0 {
                // Start of sysex: no channel voice messages until 0xF7.
                self.sysex = true;
                self.status = None;
            } else {
                self.status = Some(byte);
            }
            return;
        }
        let Some(status) = self.status else {
            return;
        };
        // Program change and channel pressure carry one data byte; everything we decode carries two.
        let needed = match status & 0xF0 {
            0xC0 | 0xD0 => 1,
            _ => 2,
        };
        self.data[self.len] = byte;
        self.len += 1;
        if self.len < needed {
            return;
        }
        self.len = 0;
        let channel = status & 0x0F;
        match status & 0xF0 {
            0x80 => out.push(Event::NoteOff {
                channel,
                key: self.data[0],
                velocity: self.data[1],
            }),
            // Velocity 0 is the running-status idiom for "note off"; normalise it here so no
            // downstream consumer has to know the convention.
            0x90 if self.data[1] == 0 => out.push(Event::NoteOff {
                channel,
                key: self.data[0],
                velocity: 0,
            }),
            0x90 => out.push(Event::NoteOn {
                channel,
                key: self.data[0],
                velocity: self.data[1],
            }),
            0xB0 => out.push(Event::ControlChange {
                channel,
                controller: self.data[0],
                value: self.data[1],
            }),
            0xE0 => {
                let raw = (i16::from(self.data[1]) << 7) | i16::from(self.data[0]);
                out.push(Event::PitchBend {
                    channel,
                    value: raw - 8192,
                });
            }
            // 0xA0 poly aftertouch and 0xD0 channel pressure are not mapped in v1.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_channel_voice_messages_v1_maps() {
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.feed(&[0x90, 60, 100, 0x80, 60, 0]),
            vec![
                Event::NoteOn { channel: 0, key: 60, velocity: 100 },
                Event::NoteOff { channel: 0, key: 60, velocity: 0 },
            ]
        );
        assert_eq!(
            decoder.feed(&[0xB3, 7, 64]),
            vec![Event::ControlChange { channel: 3, controller: 7, value: 64 }]
        );
        // Bend is 14-bit, centred at 8192.
        assert_eq!(
            decoder.feed(&[0xE0, 0x00, 0x40]),
            vec![Event::PitchBend { channel: 0, value: 0 }]
        );
        assert_eq!(
            decoder.feed(&[0xE0, 0x00, 0x00]),
            vec![Event::PitchBend { channel: 0, value: -8192 }]
        );
        assert_eq!(
            decoder.feed(&[0xE0, 0x7F, 0x7F]),
            vec![Event::PitchBend { channel: 0, value: 8191 }]
        );
    }

    #[test]
    fn note_on_velocity_zero_is_a_note_off() {
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.feed(&[0x91, 48, 0]),
            vec![Event::NoteOff { channel: 1, key: 48, velocity: 0 }]
        );
    }

    #[test]
    fn running_status_reuses_the_last_status_byte() {
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.feed(&[0x90, 60, 100, 61, 100, 62, 0]),
            vec![
                Event::NoteOn { channel: 0, key: 60, velocity: 100 },
                Event::NoteOn { channel: 0, key: 61, velocity: 100 },
                Event::NoteOff { channel: 0, key: 62, velocity: 0 },
            ]
        );
    }

    #[test]
    fn realtime_and_sysex_noise_is_dropped() {
        let mut decoder = Decoder::new();
        // Clock bytes interleaved mid-message must not disturb the data.
        assert_eq!(
            decoder.feed(&[0xF8, 0x90, 0xF8, 60, 0xFE, 100]),
            vec![Event::NoteOn { channel: 0, key: 60, velocity: 100 }]
        );
        // A sysex dump is swallowed whole, including its payload bytes.
        assert_eq!(
            decoder.feed(&[0xF0, 0x7E, 0x00, 0x06, 0x01, 0xF7]),
            vec![]
        );
        // ... and the stream resumes cleanly afterwards.
        assert_eq!(
            decoder.feed(&[0xB0, 7, 1]),
            vec![Event::ControlChange { channel: 0, controller: 7, value: 1 }]
        );
    }

    #[test]
    fn a_truncated_message_waits_for_the_rest() {
        let mut decoder = Decoder::new();
        assert!(decoder.feed(&[0x90, 60]).is_empty());
        assert_eq!(
            decoder.feed(&[100]),
            vec![Event::NoteOn { channel: 0, key: 60, velocity: 100 }]
        );
    }

    #[test]
    fn data_before_any_status_is_ignored() {
        let mut decoder = Decoder::new();
        assert!(decoder.feed(&[60, 100, 127]).is_empty());
    }

    #[test]
    fn unmapped_channel_messages_yield_nothing_but_keep_the_stream_in_sync() {
        let mut decoder = Decoder::new();
        // Program change (1 data byte) then a CC: the parser must consume exactly one byte of data.
        assert_eq!(
            decoder.feed(&[0xC0, 5, 0xB0, 7, 9]),
            vec![Event::ControlChange { channel: 0, controller: 7, value: 9 }]
        );
    }
}
