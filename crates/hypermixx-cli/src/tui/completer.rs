//! Context-aware completion for the TUI command line.
//!
//! Pure functions over the current buffer: given the text, the cursor and a little context, return
//! the range to replace and the candidates for it. The grammar mirrors `command.rs`: an optional
//! leading target (`deck0` / `0` / `master`), then a verb, then its arguments. Slot candidates carry
//! the effect name as the primary text and the index as a hint, so `fx remove` is usable without
//! memorising numbers.

use std::collections::HashMap;
use std::ops::Range;

use hypermixx_audio::fx::FxKind;

/// One completion row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    /// Short right-hand annotation, e.g. `slot 2` or `decode a file`.
    pub hint: String,
}

/// The result of a completion query: text to replace and what to offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    /// Character-index range in the buffer covering the token being completed.
    pub replace: Range<usize>,
    pub items: Vec<Candidate>,
}

/// Dynamic inputs the completer cannot know on its own.
pub struct Ctx<'a> {
    pub decks: usize,
    /// Chain the focused deck targets, e.g. `"deck0"`.
    pub chain: String,
    /// Chain label -> slot kinds, from the last `fx list`.
    pub slots: &'a HashMap<String, Vec<String>>,
}

const VERBS: &[(&str, &str)] = &[
    ("load", "decode a file"),
    ("analyse", "run the analyser"),
    ("play", "start deck"),
    ("pause", "stop deck"),
    ("jump", "seek to frame"),
    ("beatjump", "seek by beats"),
    ("rate", "set tempo rate"),
    ("profile", "time-stretch profile"),
    ("fx", "effect chain commands"),
    ("state", "show decks"),
    ("zoom", "waveform zoom"),
    ("help", "list commands"),
    ("quit", "exit"),
];

const FX_SUBS: &[(&str, &str)] = &[
    ("add", "append an effect"),
    ("remove", "drop a slot"),
    ("list", "show slots and names"),
    ("set", "set a parameter"),
    ("on", "engage a slot"),
    ("off", "bypass a slot"),
    ("trigger", "fire the one-shot"),
    ("pad", "momentary hold"),
    ("help", "effect reference"),
];

/// Completes the token under `cursor`. `None` when there is nothing useful to offer.
pub fn complete(command: &[char], cursor: usize, ctx: &Ctx) -> Option<Completion> {
    let length = command.len();
    let cursor = cursor.min(length);

    // The token under the cursor: its start, and the end of the whole (possibly longer) word.
    let mut start = cursor;
    while start > 0 && !command[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = cursor;
    while end < length && !command[end].is_whitespace() {
        end += 1;
    }
    let partial: String = command[start..cursor].iter().collect();
    let prior: Vec<String> = split_words(&command[..start]);

    let items = candidates(&prior, &partial, ctx);
    if items.is_empty() {
        None
    } else {
        Some(Completion {
            replace: start..end,
            items,
        })
    }
}

fn candidates(prior: &[String], partial: &str, ctx: &Ctx) -> Vec<Candidate> {
    // Peel off a leading target word, if any.
    let (chain, rest) = match prior.first() {
        Some(token) => match parse_target(token, ctx.decks) {
            Some(Target::Deck(deck_id)) => (format!("deck{deck_id}"), &prior[1..]),
            Some(Target::Master) => ("master".to_owned(), &prior[1..]),
            None => (ctx.chain.clone(), prior),
        },
        None => (ctx.chain.clone(), prior),
    };
    let master = chain == "master";

    // Still completing the verb (or the target word itself).
    if rest.is_empty() {
        if master {
            return pairs(&[("fx", "effect chain commands")], partial);
        }
        let mut items = pairs(VERBS, partial);
        items.extend(targets(ctx.decks, partial));
        return items;
    }

    match rest[0].as_str() {
        "load" => {
            if rest.len() == 1 {
                path_candidates(partial)
            } else {
                Vec::new()
            }
        }
        "profile" => {
            if rest.len() == 1 {
                pairs(&[("tape", ""), ("keylock", ""), ("wide", "")], partial)
            } else {
                Vec::new()
            }
        }
        "fx" => fx_candidates(&rest[1..], partial, &chain, ctx),
        "zoom" => {
            if rest.len() == 1 {
                pairs(&[("in", "zoom in"), ("out", "zoom out"), ("fit", "fit the track")], partial)
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

fn fx_candidates(rel: &[String], partial: &str, chain: &str, ctx: &Ctx) -> Vec<Candidate> {
    let Some(sub) = rel.first() else {
        return pairs(FX_SUBS, partial);
    };
    let kinds = ctx.slots.get(chain).map(Vec::as_slice).unwrap_or(&[]);
    match (sub.as_str(), rel.len()) {
        ("add", 1) => fx_kinds(partial),
        ("remove" | "rm", 1)
        | ("on" | "off", 1)
        | ("trigger", 1)
        | ("set", 1)
        | ("pad", 1) => slot_candidates(kinds, partial),
        ("set", 2) => slot_param_candidates(kinds, &rel[1], partial),
        ("pad", 2) => pairs(&[("press", ""), ("release", "")], partial),
        _ => Vec::new(),
    }
}

/// Slot rows: the effect name (hint `slot N`) and the bare index (hint the kind).
fn slot_candidates(kinds: &[String], partial: &str) -> Vec<Candidate> {
    let mut items = Vec::new();
    for (index, kind) in kinds.iter().enumerate() {
        items.push(Candidate {
            text: kind.clone(),
            hint: format!("slot {index}"),
        });
        items.push(Candidate {
            text: index.to_string(),
            hint: kind.clone(),
        });
    }
    items.retain(|candidate| candidate.text.starts_with(partial));
    items
}

/// Parameter names for the slot named or indexed by `token`.
fn slot_param_candidates(kinds: &[String], token: &str, partial: &str) -> Vec<Candidate> {
    let Some(kind) = resolve_kind(kinds, token) else {
        return Vec::new();
    };
    FxKind::parse(kind)
        .ok()
        .map(|kind| {
            kind.param_names()
                .iter()
                .filter(|name| name.starts_with(partial))
                .map(|name| Candidate {
                    text: name.to_string(),
                    hint: kind.name().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn resolve_kind<'a>(kinds: &'a [String], token: &str) -> Option<&'a str> {
    if let Ok(index) = token.parse::<usize>() {
        return kinds.get(index).map(String::as_str);
    }
    kinds
        .iter()
        .find(|kind| kind.eq_ignore_ascii_case(token))
        .map(String::as_str)
}

/// The effect names the engine actually builds, straight from the registry.
fn fx_kinds(partial: &str) -> Vec<Candidate> {
    FxKind::ALL
        .iter()
        .map(|kind| Candidate {
            text: kind.name().to_owned(),
            hint: kind.param_names().join(", "),
        })
        .filter(|candidate| candidate.text.starts_with(partial))
        .collect()
}

/// Leading target words, so a non-focused deck or the master chain is reachable from completion.
fn targets(decks: usize, partial: &str) -> Vec<Candidate> {
    let mut items = vec![Candidate {
        text: "master".to_owned(),
        hint: "master fx".to_owned(),
    }];
    for index in 0..decks {
        items.push(Candidate {
            text: format!("deck{index}"),
            hint: "target deck".to_owned(),
        });
    }
    items.retain(|candidate| candidate.text.starts_with(partial));
    items
}

/// Filesystem completion for the `load` path. Only directory reads, capped so a huge folder cannot
/// stall the UI.
fn path_candidates(partial: &str) -> Vec<Candidate> {
    let (dir, prefix) = match partial.rfind('/') {
        Some(index) => (&partial[..=index], &partial[index + 1..]),
        None => ("", partial),
    };
    let read = if dir.is_empty() { "." } else { dir };
    let Ok(entries) = std::fs::read_dir(read) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(prefix) {
            continue;
        }
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        out.push(Candidate {
            text: format!("{dir}{name}{}", if is_dir { "/" } else { "" }),
            hint: if is_dir { "dir".to_owned() } else { String::new() },
        });
        if out.len() >= 200 {
            break;
        }
    }
    out.sort_by(|a, b| a.text.cmp(&b.text));
    out
}

enum Target {
    Deck(usize),
    Master,
}

/// Mirrors `command::parse_target` for completion purposes.
fn parse_target(token: &str, decks: usize) -> Option<Target> {
    let lower = token.to_ascii_lowercase();
    if lower == "master" || lower == "m" {
        return Some(Target::Master);
    }
    let digits = lower
        .strip_prefix("deck")
        .or_else(|| lower.strip_prefix('d'))
        .unwrap_or(&lower);
    let id: usize = digits.parse().ok()?;
    (id < decks).then_some(Target::Deck(id))
}

fn split_words(chars: &[char]) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for ch in chars {
        if ch.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(*ch);
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn pairs(list: &[(&str, &str)], partial: &str) -> Vec<Candidate> {
    list.iter()
        .filter(|(text, _)| text.starts_with(partial))
        .map(|(text, hint)| Candidate {
            text: (*text).to_owned(),
            hint: (*hint).to_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        slots: HashMap<String, Vec<String>>,
    }

    impl Fixture {
        fn new() -> Self {
            let mut slots = HashMap::new();
            slots.insert(
                "deck0".to_owned(),
                vec!["eq".to_owned(), "filter".to_owned()],
            );
            Self { slots }
        }

        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                decks: 2,
                chain: "deck0".to_owned(),
                slots: &self.slots,
            }
        }

        fn complete(&self, buffer: &str, cursor: usize) -> Vec<String> {
            let chars: Vec<char> = buffer.chars().collect();
            complete(&chars, cursor, &self.ctx())
                .map(|completion| {
                    completion
                        .items
                        .into_iter()
                        .map(|candidate| candidate.text)
                        .collect()
                })
                .unwrap_or_default()
        }
    }

    #[test]
    fn root_words() {
        let fixture = Fixture::new();
        assert_eq!(fixture.complete("lo", 2), vec!["load"]);
        assert!(fixture.complete("p", 1).contains(&"play".to_owned()));
        assert!(fixture.complete("ma", 2).contains(&"master".to_owned()));
    }

    #[test]
    fn master_only_takes_fx() {
        let fixture = Fixture::new();
        assert_eq!(fixture.complete("master ", 7), vec!["fx"]);
    }

    #[test]
    fn profile_names_need_no_deck() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.complete("profile ", 8),
            vec!["tape", "keylock", "wide"]
        );
    }

    #[test]
    fn fx_subcommands_and_kinds() {
        let fixture = Fixture::new();
        assert!(fixture.complete("fx a", 4).contains(&"add".to_owned()));
        assert!(fixture.complete("fx add ", 7).contains(&"eq".to_owned()));
    }

    #[test]
    fn fx_slots_offer_names_and_indices() {
        let fixture = Fixture::new();
        let items = fixture.complete("fx remove ", 10);
        assert!(items.contains(&"eq".to_owned()));
        assert!(items.contains(&"filter".to_owned()));
        assert!(items.contains(&"0".to_owned()));
        assert!(items.contains(&"1".to_owned()));
    }

    #[test]
    fn fx_params_resolve_a_slot_name() {
        let fixture = Fixture::new();
        let params = fixture.complete("fx set eq ", 10);
        assert!(params.contains(&"low".to_owned()), "eq params: {params:?}");
    }

    #[test]
    fn target_switches_the_chain() {
        let fixture = Fixture::new();
        // deck1 has no cached slots, so only the subcommands are known there.
        assert!(fixture.complete("deck1 fx ", 9).contains(&"add".to_owned()));
    }

    #[test]
    fn replace_range_covers_the_token() {
        let fixture = Fixture::new();
        let chars: Vec<char> = "lo".chars().collect();
        let completion = complete(&chars, 2, &fixture.ctx()).unwrap();
        assert_eq!(completion.replace, 0..2);
    }
}
