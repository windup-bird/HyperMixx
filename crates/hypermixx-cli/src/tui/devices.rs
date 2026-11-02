//! The rows a device picker shows.
//!
//! A stereo interface gives one row. A 4-channel one gives a row per **channel pair**, because that
//! is the real choice a DJ makes on a multi-output interface: master on `0,1` and cue on `2,3` of the
//! same box. Both the settings screen and the controller guide draw this list, so the row model lives
//! here rather than in either of them.

use hypermixx_audio::DeviceInfo;

/// One selectable row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// The device itself: bind it and keep whatever channel pair it had (or the first one).
    Device { index: usize },
    /// One stereo pair of a wider device: bind it *and* the pair.
    Pair { index: usize, channels: (u16, u16) },
}

impl Row {
    pub fn device(self) -> usize {
        match self {
            Row::Device { index } | Row::Pair { index, .. } => index,
        }
    }
}

/// The selectable rows for `devices`, in display order.
///
/// A device with four to eight channels contributes its own row followed by one row per stereo pair,
/// so "this interface, channels 2 and 3" is one Enter away. A *virtual* device that claims more than
/// that (`sysdefault` under ALSA advertises 32 channels for anything) is not expanded: sixteen
/// meaningless "channels 2,3" rows would bury the real interfaces.
/// The whole list in one column: each device, and (for a 4–8 channel interface) one row per stereo
/// pair under it. The settings screen and the guide keep the two levels apart instead — hardware,
/// then channels — so this is the merged view the tests pin down.
#[cfg(test)]
pub fn rows(devices: &[DeviceInfo]) -> Vec<Row> {
    let mut out = Vec::new();
    for (index, device) in devices.iter().enumerate() {
        out.push(Row::Device { index });
        if channel_pairs(device.channels).len() > 1 {
            for pair in channel_pairs(device.channels) {
                out.push(Row::Pair {
                    index,
                    channels: pair,
                });
            }
        }
    }
    out
}

/// The stereo pairs of a device with `channels` outputs: `(0,1)`, `(2,3)`, `(4,5)`, …
pub fn pairs(channels: u16) -> Vec<(u16, u16)> {
    (0..channels / 2)
        .map(|pair| (pair * 2, pair * 2 + 1))
        .collect()
}

/// The pairs worth **offering** for a device that advertises `channels` outputs.
///
/// A real interface reports 2, 4, 6 or 8 channels, and every pair is a real socket. ALSA's `plug`
/// layer (which is what `sysdefault`/`default`/`pipewire` resolve to) accepts up to 32 channels for
/// any card, so a 4-channel USB interface advertises 32 and would otherwise produce sixteen
/// channel-pair rows of which only two exist. The first four pairs are the ones a DJ rig can use
/// (stereo, two stereo pairs, 5.1, 7.1); past that the number is a plugin detail, not hardware.
pub fn channel_pairs(channels: u16) -> Vec<(u16, u16)> {
    let offered = pairs(channels.min(MAX_PAIR_CHANNELS));
    if offered.is_empty() {
        // A device that will not tell us its count still needs its usual stereo pair.
        vec![(0, 1)]
    } else {
        offered
    }
}

/// The widest advertised channel count that is treated as hardware rather than a plugin's ceiling.
const MAX_PAIR_CHANNELS: u16 = 8;

/// The row's text: the device name (plus its card description), or `channels 2,3` under it.
pub fn label(devices: &[DeviceInfo], row: Row) -> String {
    match row {
        Row::Device { index } => devices.get(index).map_or_else(
            || "<device>".to_owned(),
            |device| {
                let default = if device.is_default { " (default)" } else { "" };
                // The card description is what makes a USB interface recognisable: the PCM name is
                // often just its card id (`sysdefault:CARD=III`).
                let card = if device.description.is_empty() || device.description == device.name {
                    String::new()
                } else {
                    format!(" — {}", device.description)
                };
                format!("{}{default}{card}", device.name)
            },
        ),
        Row::Pair { channels, .. } => format!("  channels {},{}", channels.0, channels.1),
    }
}

/// Level one of a chooser: one row per piece of hardware, with no channel pairs mixed in.
pub fn hardware_rows(devices: &[DeviceInfo]) -> Vec<Row> {
    (0..devices.len())
        .map(|index| Row::Device { index })
        .collect()
}

/// Whether `device` is plausibly the controller the map names.
///
/// The two names come from different stacks — the map says "Numark Party Mix III MIDI 1" (its port),
/// the audio host says `sysdefault:CARD=III` with the card description — so they are compared on the
/// words that actually identify the box: every *distinctive* word of four letters or more has to
/// appear somewhere in the device's name or description. Short words ("III", "Mix") and protocol
/// words ("MIDI", "USB", "Audio") are ignored, because they match everything or nothing.
pub fn matches_controller(device: &DeviceInfo, controller: &str) -> bool {
    const GENERIC: [&str; 8] = [
        "midi", "usb", "audio", "device", "port", "jack", "output", "default",
    ];
    let haystack = format!("{} {}", device.name, device.description).to_ascii_lowercase();
    let words: Vec<String> = controller
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|word| word.len() >= 4 && !GENERIC.contains(&word.as_str()))
        .collect();
    !words.is_empty() && words.iter().all(|word| haystack.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, channels: u16) -> DeviceInfo {
        DeviceInfo {
            name: name.to_owned(),
            description: String::new(),
            is_default: false,
            rates: "44100".to_owned(),
            accepts: Some(true),
            channels,
        }
    }

    #[test]
    fn a_stereo_device_is_one_row_and_a_wider_one_lists_its_pairs() {
        let devices = vec![device("Stereo", 2), device("Quad", 4)];
        let rows = rows(&devices);
        assert_eq!(
            rows,
            vec![
                Row::Device { index: 0 },
                Row::Device { index: 1 },
                Row::Pair {
                    index: 1,
                    channels: (0, 1)
                },
                Row::Pair {
                    index: 1,
                    channels: (2, 3)
                },
            ]
        );
        assert_eq!(label(&devices, rows[0]), "Stereo");
        assert_eq!(label(&devices, rows[3]), "  channels 2,3");
    }

    #[test]
    fn the_controller_is_recognised_by_its_name_words() {
        let party = DeviceInfo {
            name: "sysdefault:CARD=III".to_owned(),
            description: "Numark Party Mix III".to_owned(),
            is_default: false,
            rates: "44100".to_owned(),
            accepts: Some(true),
            channels: 4,
        };
        let headphones = DeviceInfo {
            name: "USB Headset".to_owned(),
            description: String::new(),
            ..party.clone()
        };
        assert!(matches_controller(&party, "Numark Party Mix III MIDI 1"));
        assert!(matches_controller(&party, "Numark Party Mix III"));
        assert!(!matches_controller(&headphones, "Numark Party Mix III"));
        assert!(!matches_controller(&headphones, ""));
        // Short words alone never identify a device.
        assert!(!matches_controller(&headphones, "USB III"));
    }

    #[test]
    fn a_six_channel_device_offers_three_pairs() {
        assert_eq!(pairs(6), vec![(0, 1), (2, 3), (4, 5)]);
        assert_eq!(pairs(2), vec![(0, 1)]);
    }

    #[test]
    fn a_plugins_32_channel_ceiling_does_not_become_sixteen_rows() {
        // `sysdefault:CARD=III` is a 4-channel interface behind ALSA's `plug`, whose ceiling is 32.
        // Only the pairs a real rig can have are offered, and the first two are the ones a DJ needs.
        assert_eq!(
            channel_pairs(32),
            vec![(0, 1), (2, 3), (4, 5), (6, 7)],
            "capped, but 2,3 stays reachable"
        );
        assert_eq!(channel_pairs(4), vec![(0, 1), (2, 3)]);
        // A device that reports nothing usable still gets its stereo pair.
        assert_eq!(channel_pairs(0), vec![(0, 1)]);
        assert_eq!(channel_pairs(1), vec![(0, 1)]);
    }

    #[test]
    fn a_virtual_device_that_claims_32_channels_is_not_expanded() {
        // ALSA's `sysdefault`/`pulse` plugins advertise 32 channels whatever the card is; expanding
        // them would bury the real interfaces under sixteen "channels n,n+1" rows.
        let devices = vec![device("sysdefault", 32), device("Quad", 4)];
        let rows = rows(&devices);
        assert_eq!(rows[0], Row::Device { index: 0 });
        // The pair rows under it are the capped set, not sixteen.
        assert_eq!(
            rows[1],
            Row::Pair {
                index: 0,
                channels: (0, 1)
            }
        );
        assert_eq!(
            rows[4],
            Row::Pair {
                index: 0,
                channels: (6, 7)
            }
        );
        assert_eq!(rows[5], Row::Device { index: 1 });
        assert_eq!(
            rows[6],
            Row::Pair {
                index: 1,
                channels: (0, 1)
            }
        );
        assert_eq!(
            rows[7],
            Row::Pair {
                index: 1,
                channels: (2, 3)
            }
        );
        assert_eq!(
            rows.len(),
            8,
            "four pairs under the virtual device, two under the quad: {rows:?}"
        );
    }
}
