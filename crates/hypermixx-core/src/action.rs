//! The action vocabulary, shared by every front-end that can drive the engine.
//!
//! There is **one** table, and its names *are* the session grammar: a MIDI binding's `action` is a
//! command line without the target (`"play"`, `"beatjump 4"`, `"sync phase pid"`, `"vocals mute"`),
//! and the binding's `deck` supplies the target it leaves out. A line that starts with its own
//! target word (`master fx toggle filter`) addresses that instead.
//!
//! The point of living here rather than in the mapping crate is that the mapping crate, the guide's
//! target list and the CLI's help all read it: a verb added once shows up in all three. If the table
//! and the CLI ever disagreed, this crate could not tell — that is the CLI's own test's job.

/// One argument an action line may carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArgSpec {
    /// The placeholder name, as the documentation spells it (`beats`, `mode`, `position`).
    pub name: &'static str,
    /// Whether the line is refused without it.
    pub required: bool,
    /// What the argument may be, as a human-readable list.
    pub values: &'static str,
}

/// One verb of the grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionSpec {
    /// The command line without its target, exactly as a `[[bind]]`'s `action` writes it.
    pub name: &'static str,
    /// Whether the verb needs a target. `false` means it is global (`fader master`, `state`), or
    /// that it addresses something the line itself names.
    pub deck: bool,
    /// Whether a *note* binding acts on both edges (a held cue button, a vinyl platter).
    pub momentary: bool,
    /// Arguments the line may carry, in order.
    pub args: &'static [ArgSpec],
    /// One line of prose, for the guide and the documentation.
    pub doc: &'static str,
}

const fn arg(name: &'static str, required: bool, values: &'static str) -> ArgSpec {
    ArgSpec {
        name,
        required,
        values,
    }
}

/// Every verb the engine's front-ends can drive, in the order a guide should list them.
///
/// The `<stem>` placeholders stand for `drums | bass | other | vocals`; the parser accepts any stem
/// name and the guide lists the `vocals` spelling, exactly as it does for `<stem>` levels.
pub const ACTIONS: &[ActionSpec] = &[
    ActionSpec {
        name: "play",
        deck: true,
        momentary: false,
        args: &[],
        doc: "toggle play/pause",
    },
    ActionSpec {
        name: "cue",
        deck: true,
        momentary: true,
        args: &[],
        doc: "held cue button: press = cue the playhead, release = return to it",
    },
    ActionSpec {
        name: "vinyl",
        deck: true,
        momentary: true,
        args: &[arg("frames_per_tick", false, "frames")],
        doc: "platter: a note is the touch, a relative CC is the wheel",
    },
    ActionSpec {
        name: "hotcue",
        deck: true,
        momentary: false,
        args: &[
            arg("set|trigger|clear", true, "the operation"),
            arg("slot", true, "1..8 (A..H)"),
        ],
        doc: "set, trigger, or clear one hot-cue slot",
    },
    ActionSpec {
        name: "beatjump",
        deck: true,
        momentary: false,
        args: &[arg("beats", false, "a signed whole number")],
        doc: "seek by whole beats, keeping the phase",
    },
    ActionSpec {
        name: "fader flow",
        deck: true,
        momentary: false,
        args: &[],
        doc: "the channel's per-stream level",
    },
    ActionSpec {
        name: "vocals fader",
        deck: true,
        momentary: false,
        args: &[],
        doc: "one stem's level (`<stem> fader` for any stem)",
    },
    ActionSpec {
        name: "vocals mute",
        deck: true,
        momentary: false,
        args: &[],
        doc: "latch one stem's mute (`<stem> mute`)",
    },
    ActionSpec {
        name: "vocals solo",
        deck: true,
        momentary: false,
        args: &[],
        doc: "latch one stem's solo membership (`<stem> solo`)",
    },
    ActionSpec {
        name: "fader deck",
        deck: true,
        momentary: false,
        args: &[],
        doc: "the channel's deck level",
    },
    ActionSpec {
        name: "fader gain",
        deck: true,
        momentary: false,
        args: &[],
        doc: "the channel trim (0 = 0 dB, 1 = +6 dB)",
    },
    ActionSpec {
        name: "fader cuesend",
        deck: true,
        momentary: false,
        args: &[],
        doc: "how much of the channel reaches the cue bus",
    },
    ActionSpec {
        name: "fader cross",
        deck: false,
        momentary: false,
        args: &[],
        doc: "the shared crossfader",
    },
    ActionSpec {
        name: "fader master",
        deck: false,
        momentary: false,
        args: &[],
        doc: "master trim",
    },
    ActionSpec {
        name: "fader cue",
        deck: false,
        momentary: false,
        args: &[],
        doc: "cue bus trim",
    },
    ActionSpec {
        name: "tempofader",
        deck: true,
        momentary: false,
        args: &[],
        doc: "the pitch fader position (the deck applies its own range)",
    },
    ActionSpec {
        name: "nudge",
        deck: true,
        momentary: true,
        args: &[
            arg("delta", false, "a rate, e.g. 0.15"),
            arg("seconds", false, "seconds; a timed bend releases itself"),
        ],
        doc: "temporary rate bend to align phase",
    },
    ActionSpec {
        name: "loop in",
        deck: true,
        momentary: false,
        args: &[],
        doc: "arm/engage the loop in point",
    },
    ActionSpec {
        name: "loop out",
        deck: true,
        momentary: false,
        args: &[],
        doc: "engage/retime the loop out point",
    },
    ActionSpec {
        name: "loop exit",
        deck: true,
        momentary: false,
        args: &[],
        doc: "leave the loop",
    },
    ActionSpec {
        name: "loop cancel",
        deck: true,
        momentary: false,
        args: &[],
        doc: "drop an armed in point",
    },
    ActionSpec {
        name: "loop halve",
        deck: true,
        momentary: false,
        args: &[],
        doc: "halve the running loop",
    },
    ActionSpec {
        name: "loop double",
        deck: true,
        momentary: false,
        args: &[],
        doc: "double the running loop",
    },
    ActionSpec {
        name: "loop",
        deck: true,
        momentary: false,
        args: &[arg("beats", false, "a whole number of beats")],
        doc: "beat loop of `beats` beats (default 4)",
    },
    ActionSpec {
        name: "sync tempo",
        deck: true,
        momentary: false,
        args: &[],
        doc: "match the other deck's BPM once",
    },
    ActionSpec {
        name: "sync phase",
        deck: true,
        momentary: false,
        args: &[
            arg("instant|linear|pid", false, "the correction mode"),
            arg("seconds", false, "duration, for linear"),
        ],
        doc: "match BPM, then close the phase",
    },
    ActionSpec {
        name: "sync tempolock",
        deck: true,
        momentary: false,
        args: &[],
        doc: "both decks share one tempo",
    },
    ActionSpec {
        name: "sync phaselock",
        deck: true,
        momentary: false,
        args: &[
            arg("instant|linear|pid", false, "the correction mode"),
            arg("seconds", false, "duration, for linear"),
        ],
        doc: "follow the leader exactly",
    },
    ActionSpec {
        name: "sync set-leader",
        deck: true,
        momentary: false,
        args: &[],
        doc: "name this deck as the one the others follow",
    },
    ActionSpec {
        name: "sync unlock",
        deck: true,
        momentary: false,
        args: &[],
        doc: "drop the lock and the correction, keeping the tempo",
    },
    ActionSpec {
        name: "fx on",
        deck: false,
        momentary: false,
        args: &[],
        doc: "engage a slot (`chain` + `fx` fields address it)",
    },
    ActionSpec {
        name: "fx off",
        deck: false,
        momentary: false,
        args: &[],
        doc: "bypass a slot",
    },
    ActionSpec {
        name: "fx toggle",
        deck: false,
        momentary: false,
        args: &[],
        doc: "flip a slot",
    },
    ActionSpec {
        name: "fx pad",
        deck: false,
        momentary: true,
        args: &[],
        doc: "hold a slot engaged",
    },
    ActionSpec {
        name: "fx trigger",
        deck: false,
        momentary: false,
        args: &[],
        doc: "fire a slot's one-shot hook",
    },
    ActionSpec {
        name: "fx param",
        deck: false,
        momentary: false,
        args: &[],
        doc: "drive one parameter (`chain` + `fx` + `param` fields)",
    },
    ActionSpec {
        name: "browser up",
        deck: false,
        momentary: false,
        args: &[],
        doc: "move the browser selection up",
    },
    ActionSpec {
        name: "browser down",
        deck: false,
        momentary: false,
        args: &[],
        doc: "move the browser selection down",
    },
    ActionSpec {
        name: "browser open",
        deck: false,
        momentary: false,
        args: &[],
        doc: "open the selected list / enter the track pane",
    },
    ActionSpec {
        name: "browser exit",
        deck: false,
        momentary: false,
        args: &[],
        doc: "leave the track pane / go up one list",
    },
    ActionSpec {
        name: "browser load",
        deck: false,
        momentary: false,
        args: &[],
        doc: "load the selected track (optional binding `deck`)",
    },
    ActionSpec {
        name: "browser refresh",
        deck: false,
        momentary: false,
        args: &[],
        doc: "rescan library sources",
    },
];

/// The canonical name of an action, resolving the **legacy** dot-spelled names a pre-unification
/// map may still carry (`fader.flow`, `beatjump+`, `stem.vocals.mute`, `sync.leader`, …).
///
/// One version of compatibility, deliberately: the file is user-editable, and a map that silently
/// stops working after an upgrade is worse than one that keeps a few old spellings.
pub fn canonical(name: &str) -> Option<String> {
    let name = name.trim();
    if ACTIONS.iter().any(|spec| spec.name == name) {
        return Some(name.to_owned());
    }
    // A line with arguments whose verb is in the registry (`loop 8`, `sync phase pid`,
    // `beatjump -4`, `vinyl 1323`): the verb is what has to be known, the words are the parser's
    // business.
    if ACTIONS.iter().any(|spec| {
        name.len() > spec.name.len()
            && name.starts_with(spec.name)
            && name[spec.name.len()..].starts_with(char::is_whitespace)
    }) {
        return Some(name.to_owned());
    }
    if let Some((head, rest)) = name.split_once(char::is_whitespace) {
        // A stem line: `<stem> fader | mute | solo`, for any of the four stems (`vocals mute` is in
        // the registry as the guide's spelling; `drums mute` is the same verb).
        if matches!(rest.trim(), "fader" | "mute" | "solo")
            && crate::stem::Stem::parse(head).is_some()
        {
            return Some(name.to_owned());
        }
        // `<target> fx <verb>`: the line names its own chain, the way a session line's leading
        // target word does (`deck0 vocals fx toggle filter`, `master fx off limiter`).
        if let Some(verb) = rest.trim().strip_prefix("fx ") {
            let verb = verb.trim();
            let known = ACTIONS.iter().any(|spec| {
                spec.name.strip_prefix("fx ") == Some(verb)
                    || (verb == "param" && spec.name == "fx param")
            });
            if known && is_chain_target(head) {
                return Some(name.to_owned());
            }
        }
    }
    // Not in the registry, but a valid spelling nonetheless: a button that only places the cue.
    if name == "cue smart" {
        return Some(name.to_owned());
    }
    let legacy = match name {
        // The bare verb: the sign lives in the name's suffix, and the *magnitude* in the binding's
        // `step` — which a line with a number would override.
        "beatjump+" | "beatjump-" => "beatjump".to_owned(),
        "fader.flow" => "fader flow".to_owned(),
        "fader.deck" => "fader deck".to_owned(),
        "fader.gain" => "fader gain".to_owned(),
        "fader.cuesend" => "fader cuesend".to_owned(),
        "fader.cross" => "fader cross".to_owned(),
        "fader.master" => "fader master".to_owned(),
        "fader.cue" => "fader cue".to_owned(),
        "sync.leader" => "sync set-leader".to_owned(),
        "sync.tempo" => "sync tempo".to_owned(),
        "sync.phase" => "sync phase".to_owned(),
        "sync.tempolock" => "sync tempolock".to_owned(),
        "sync.phaselock" => "sync phaselock".to_owned(),
        "sync.unlock" => "sync unlock".to_owned(),
        "loop.in" => "loop in".to_owned(),
        "loop.out" => "loop out".to_owned(),
        "loop.exit" => "loop exit".to_owned(),
        "loop.cancel" => "loop cancel".to_owned(),
        "loop.halve" => "loop halve".to_owned(),
        "loop.double" => "loop double".to_owned(),
        "loop.beat" => "loop".to_owned(),
        "cue.smart" => "cue smart".to_owned(),
        "fx.on" => "fx on".to_owned(),
        "fx.off" => "fx off".to_owned(),
        "fx.toggle" => "fx toggle".to_owned(),
        "fx.pad" => "fx pad".to_owned(),
        "fx.trigger" => "fx trigger".to_owned(),
        "fx.param" => "fx param".to_owned(),
        "browser.up" => "browser up".to_owned(),
        "browser.down" => "browser down".to_owned(),
        "browser.open" => "browser open".to_owned(),
        "browser.exit" => "browser exit".to_owned(),
        "browser.load" => "browser load".to_owned(),
        "browser.refresh" => "browser refresh".to_owned(),
        other => {
            if let Some(beats) = other.strip_prefix("loop.beat") {
                let beats = beats.trim();
                if !beats.is_empty() && beats.chars().all(|c| c.is_ascii_digit()) {
                    return Some(format!("loop {beats}"));
                }
            }
            if let Some(stem) = other.strip_prefix("fader.stem.") {
                return Some(format!("{stem} fader"));
            }
            if let Some(rest) = other.strip_prefix("stem.") {
                if let Some((stem_name, what)) = rest.rsplit_once('.') {
                    // Any verb: the *parser* is what says whether it is one it knows, and naming the
                    // canonical line makes the error about the verb rather than about the spelling.
                    return Some(format!("{stem_name} {what}"));
                }
            }
            return None;
        }
    };
    Some(legacy)
}

/// Whether a word names an FX chain the way a session line's leading target does: `master`, `m`,
/// a deck (`deck0` / `d0` / `0`), or a stem name.
fn is_chain_target(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    if lower == "master" || lower == "m" {
        return true;
    }
    if crate::stem::Stem::parse(&lower).is_some() {
        return true;
    }
    let digits = lower
        .strip_prefix("deck")
        .or_else(|| lower.strip_prefix('d'))
        .unwrap_or(&lower);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// The action list as one line of help per verb, for `--help` output and the documentation.
pub fn help_lines() -> Vec<String> {
    ACTIONS
        .iter()
        .map(|spec| {
            let args: String = spec
                .args
                .iter()
                .map(|arg| {
                    if arg.required {
                        format!(" <{}>", arg.name)
                    } else {
                        format!(" [{}]", arg.name)
                    }
                })
                .collect();
            format!("  {}{args:<28} {}", spec.name, spec.doc)
        })
        .collect()
}
