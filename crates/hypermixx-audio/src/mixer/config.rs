//! [`MixerConfig`]: what a mixer is built from — channels, master/cue chains, outputs.
//!
//! Everything here is plain data (`String` effect names, `f32` fader positions): a TOML file, a
//! crate or a test helper can all produce the same thing. The file schema is [`MixerFile`] at the
//! bottom of this module; [`simple_dj`] is the built-in reference and [`reference_toml`] its text.
//!
//! FX are named by string and resolved through [`FxKind`](crate::fx::FxKind) at build time. A name
//! that is not in the registry is a [`MixerError::UnknownFx`] at construction, not a silent skip:
//! a mixer that quietly dropped the limiter the user configured is worse than one that refuses to
//! start.

use hypermixx_core::Stem;

use super::channel::{CrossfaderCurve, CrossfaderPlace, CueTap, DeckSide};
use super::output::OutputId;
use crate::fx::{FxChain, FxError, FxKind, FxSlot};
/// Why a mixer could not be built from a config.
#[derive(Clone, Debug, PartialEq)]
pub enum MixerError {
    /// An FX name is not in the registry.
    UnknownFx(String),
    /// An effect refused its initial parameter.
    BadParam { kind: String, name: String },
    /// No channels configured: the mixer would produce silence forever.
    Empty,
    /// An output could not be opened.
    Output(String),
    /// A config file could not be parsed.
    Parse(String),
}

impl MixerError {
    pub fn message(&self) -> String {
        match self {
            MixerError::UnknownFx(kind) => format!(
                "unknown effect `{kind}` (known: {})",
                FxKind::ALL
                    .iter()
                    .map(|k| k.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            MixerError::BadParam { kind, name } => {
                format!("{kind} rejected its initial parameter `{name}`")
            }
            MixerError::Empty => "no channels configured".into(),
            MixerError::Output(what) => format!("output: {what}"),
            MixerError::Parse(what) => format!("config: {what}"),
        }
    }
}

impl std::fmt::Display for MixerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for MixerError {}

impl From<FxError> for MixerError {
    fn from(err: FxError) -> Self {
        match err {
            FxError::UnknownKind(kind) => MixerError::UnknownFx(kind),
            other => MixerError::Output(other.message()),
        }
    }
}

/// One channel's built chains: one flow chain per stream ([`Stem::ALL`] order), then the shared
/// deck chain's slots. A named type because the tuple is otherwise unreadable at every use site.
pub type BuiltChains = (Vec<FxChain>, Vec<FxSlot>);

/// One mixer input.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChannelConfig {
    /// Per-stream inserts, as a **template**: every stream gets its own chain built from this list,
    /// so four stems get four independent instances (separate filter state, not one filter shared).
    ///
    /// A stem that should differ is named in [`ChannelConfig::stem_fx`]; an empty list means "no
    /// per-stream inserts", which is the default and costs nothing (the chains are never entered).
    pub flow_fx: Vec<String>,
    /// Per-stem overrides of [`ChannelConfig::flow_fx`], for the stems that should differ. A stem
    /// absent here uses the template.
    #[serde(default)]
    pub stem_fx: std::collections::HashMap<Stem, Vec<String>>,
    /// FX between the flow fader and the deck fader — the channel's tone controls.
    pub deck_fx: Vec<String>,
    /// Per-stream level, `-1.0 ..= 1.0` (centred = unity, the top of the travel is 0 dB).
    pub flow_fader: f32,
    /// Deck level; stays at unity until a track exposes stems.
    pub deck_fader: f32,
    /// Channel trim, `-1.0 ..= 1.0`: `0.0` is 0 dB, `1.0` is +6 dB (the only stage that boosts),
    /// `-1.0` is silence. The last stage before the sum.
    pub gain: f32,
    /// How much of this channel reaches the cue bus: **linear, 0.0 (off) ..= 1.0 (full)**.
    pub cue_send: f32,
    pub cue_tap: CueTap,
    pub side: DeckSide,
}

impl Default for ChannelConfig {
    /// A muted, centred, effect-free input. Anything a caller builds starts from here so a new field
    /// cannot silently change an existing config's behaviour.
    fn default() -> Self {
        Self {
            flow_fx: Vec::new(),
            stem_fx: std::collections::HashMap::new(),
            deck_fx: Vec::new(),
            flow_fader: 0.0,
            deck_fader: 0.0,
            gain: 0.0,
            cue_send: 0.0,
            cue_tap: CueTap::default(),
            side: DeckSide::default(),
        }
    }
}

impl ChannelConfig {
    /// Every FX name this channel names — the template, every override, then the deck chain. Used
    /// for validation and `fx list`, so an unknown name in an override cannot hide.
    pub fn all_fx(&self) -> impl Iterator<Item = &String> {
        self.flow_fx
            .iter()
            .chain(self.stem_fx.values().flatten())
            .chain(self.deck_fx.iter())
    }

    /// The names one stream's chain is built from: its own override, else the template.
    pub fn chain_names(&self, stem: Stem) -> &[String] {
        self.stem_fx.get(&stem).unwrap_or(&self.flow_fx)
    }

    /// A channel with `deck_fx` tone controls and no per-stream inserts.
    pub fn dj(deck_fx: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            deck_fx: deck_fx.into_iter().map(Into::into).collect(),
            ..Default::default()
        }
    }
}

/// One destination.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    /// `id` is optional in a file (and ignored: the mixer renumbers outputs by position), so a
    /// destination needs only a `name` and a `role`.
    #[serde(default)]
    pub id: OutputId,
    pub name: String,
    /// `(left, right)` device channel indices.
    #[serde(default = "default_channels")]
    pub channels: (u16, u16),
    pub role: OutputRole,
    /// Trim applied when the bus is handed over, so two destinations can differ in level without
    /// the mixer knowing about it.
    #[serde(default = "default_gain")]
    pub gain: f32,
}

#[inline]
fn default_channels() -> (u16, u16) {
    (0, 1)
}

#[inline]
fn default_gain() -> f32 {
    1.0
}

/// The crossfader's shape, shared by every channel: one physical control has one position, one
/// curve and one cut width. Configured by the `[crossfader]` table.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CrossfaderConfig {
    /// Where in the channel path the gain acts.
    pub place: CrossfaderPlace,
    pub curve: CrossfaderCurve,
    /// Fraction of the travel the `cut` curve drops over, at each end.
    pub cut_width: f32,
    /// Starting position, `-1.0` hard left .. `1.0` hard right.
    pub start: f32,
}

impl Default for CrossfaderConfig {
    fn default() -> Self {
        Self {
            place: CrossfaderPlace::Deck,
            curve: CrossfaderCurve::Classic,
            cut_width: super::DEFAULT_CUT_WIDTH,
            start: 0.0,
        }
    }
}

/// The cue bus's shape: what a `cue` output carries. Configured by the `[cue]` table.
///
/// A DJ's headphone output is a **blend** of the cue sends and the master mix, so the cue bus mixes
/// the two with a crossfader-like control. `blend = -1.0` is the default and the historical
/// behaviour: headphones carry the cue and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CueMixConfig {
    /// Cue trim.
    pub fader: f32,
    /// `-1.0` = cue only, `0.0` = both at the curve's centre, `1.0` = master only.
    pub blend: f32,
    pub curve: CrossfaderCurve,
    /// Fraction of the blend the `cut` curve drops over, at each end.
    pub cut_width: f32,
}

impl Default for CueMixConfig {
    fn default() -> Self {
        Self {
            fader: 0.0,
            blend: -1.0,
            curve: CrossfaderCurve::Classic,
            cut_width: super::DEFAULT_CUT_WIDTH,
        }
    }
}

/// Whether an output carries the mix or the cue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputRole {
    /// The summed mix.
    #[default]
    Master,
    /// The cue bus (the headphone destination).
    Cue,
}

impl Default for OutputConfig {
    /// The plain stereo default: channels (0,1), main role, unity gain. `name` still has to come
    /// from the config — an anonymous destination cannot be named in a log line.
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            channels: (0, 1),
            role: OutputRole::Master,
            gain: 1.0,
        }
    }
}

impl OutputConfig {
    /// A stereo master destination.
    pub fn master(id: OutputId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            channels: (0, 1),
            role: OutputRole::Master,
            gain: 1.0,
        }
    }

    /// A cue (headphone) destination.
    pub fn cue(id: OutputId, name: impl Into<String>) -> Self {
        Self {
            role: OutputRole::Cue,
            ..Self::master(id, name)
        }
    }

    /// The mapping to use against a device with `device_channels` outputs: the configured pair if it
    /// fits, otherwise a safe fallback. Out-of-range indices would panic in the write path.
    pub fn channel_pair(&self, device_channels: usize) -> (u16, u16) {
        let (l, r) = self.channels;
        if (l as usize) < device_channels && (r as usize) < device_channels {
            (l, r)
        } else {
            (0, 1)
        }
    }
}

/// Everything a [`Mixer`](super::Mixer) needs to exist.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MixerConfig {
    pub channels: Vec<ChannelConfig>,
    /// The shared crossfader (one physical control, every channel applies it on its own side).
    pub crossfader: CrossfaderConfig,
    /// FX on the summed mix, before the safety limiter.
    pub master_fx: Vec<String>,
    /// Always-on safety limiter, wired separately so it cannot be configured away by a bad list.
    pub master_limiter: bool,
    /// Master trim, `-1.0 ..= 1.0` (0 dB at the top).
    pub master_fader: f32,
    /// Cue trim, `-1.0 ..= 1.0`.
    pub cue_fader: f32,
    /// How a `cue` output blends the cue sends with the master mix.
    pub cue: CueMixConfig,
    pub outputs: Vec<OutputConfig>,
}

impl MixerConfig {
    /// Builds the chains this config names: **one per stream** (`Stem::COUNT` of them, from the
    /// template and any per-stem overrides) plus the shared deck chain.
    ///
    /// Split out so a mixer can report a bad FX name without having touched a device — including a
    /// bad name inside a `stem_fx` override, which would otherwise only surface when that stem's
    /// chain was first built.
    pub fn build_chains(&self) -> Result<Vec<BuiltChains>, MixerError> {
        self.channels
            .iter()
            .map(|channel| {
                let flow = Stem::ALL
                    .iter()
                    .map(|stem| build_chain(channel.chain_names(*stem)))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((flow, build_slots(&channel.deck_fx)?))
            })
            .collect()
    }

    /// Parses a mixer topology from TOML text.
    ///
    /// This is the parser half of the "config is plain data" rule: the file schema is spelled out
    /// by [`MixerFile`] right below — a small mirror of [`MixerConfig`] whose only job is naming
    /// (`[[channel]]` singular, a `[master]` table), so the Rust type and the file format can each
    /// keep the names that suit them. File IO stays with the caller — a front-end decides where
    /// topologies come from, the engine only reads the text.
    ///
    /// A config that parses but names an unknown effect still succeeds here; that is a
    /// *construction* error ([`MixerError::UnknownFx`] from `build_chains`), surfaced when a mixer
    /// is built rather than when the text is read, because that is where the registry is consulted.
    pub fn from_toml_str(text: &str) -> Result<Self, MixerError> {
        let file: MixerFile = toml::from_str(text).map_err(|err| MixerError::Parse(err.to_string()))?;
        Ok(file.into())
    }
}

/// The TOML schema for a whole mixer: one `[[channel]]` per deck, a `[crossfader]`, a `[master]`,
/// a `[cue]` and any number of `[[output]]`s. Every field defaults, so the smallest legal file is a
/// single `[[channel]]`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct MixerFile {
    channel: Vec<ChannelConfig>,
    crossfader: CrossfaderConfig,
    master: MasterFile,
    cue: CueMixConfig,
    output: Vec<OutputConfig>,
}

/// The `[master]` table: `fx`, `limiter`, `fader`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct MasterFile {
    fx: Vec<String>,
    /// The safety stage. Opt-in from a file (the built-in reference turns it on; a hand-written
    /// config that omits it gets exactly what it asked for, and the mixer docs say so).
    limiter: bool,
    fader: f32,
}

impl From<MixerFile> for MixerConfig {
    fn from(file: MixerFile) -> Self {
        let cue = file.cue;
        Self {
            channels: file.channel,
            crossfader: file.crossfader,
            master_fx: file.master.fx,
            master_limiter: file.master.limiter,
            master_fader: file.master.fader,
            cue_fader: cue.fader,
            cue,
            outputs: file.output,
        }
    }
}

/// The reference topology: two decks, each with an EQ and a sweep, a limiting master, and master plus
/// cue (headphone) destinations.
///
/// This is the function every test and the CLI starts from. If it changes, the acceptance question
/// ("does a cue still work, does the crossfader still isolate a deck?") has a new answer.
pub fn simple_dj() -> MixerConfig {
    let deck_channels = |side: DeckSide| ChannelConfig {
        flow_fx: vec![],
        // The reference topology has no per-stem inserts: stems get the same (empty) template, and
        // `[channel.stem_fx]` is how a user asks for one stem to differ.
        stem_fx: std::collections::HashMap::new(),
        deck_fx: vec!["eq".into(), "filter".into()],
        flow_fader: 0.0,
        deck_fader: 0.0,
        gain: 0.0,
        cue_send: 1.0,
        cue_tap: CueTap::PostDeckFader,
        side,
    };
    MixerConfig {
        channels: vec![
            deck_channels(DeckSide::Left),
            deck_channels(DeckSide::Right),
        ],
        crossfader: CrossfaderConfig::default(),
        master_fx: vec![],
        master_limiter: true,
        master_fader: 0.0,
        cue_fader: 0.0,
        cue: CueMixConfig::default(),
        outputs: vec![
            OutputConfig::master(0, "main"),
            OutputConfig::cue(1, "headphones"),
        ],
    }
}

/// The reference topology as TOML — the starting point for a custom config (`--print-config`
/// emits it). Parsing this text must yield exactly [`simple_dj`], which is the test that keeps the
/// schema and the constructor honest with each other.
pub fn reference_toml() -> &'static str {
    // The reference topology ships as a file so there is exactly one source of truth: this text,
    // `--print-config` and `config/mixer.toml` are the same bytes.
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/mixer.toml"))
}

/// A single-channel config with no outputs at all: the mixer computes and writes nothing, which is
/// what makes [`Mixer::process`](super::Mixer::process) unit-testable without a sound card.
pub fn silent_test_channel() -> MixerConfig {
    MixerConfig {
        channels: vec![ChannelConfig {
            flow_fx: vec![],
            stem_fx: std::collections::HashMap::new(),
            deck_fx: vec![],
            flow_fader: 0.0,
            deck_fader: 0.0,
            gain: 0.0,
            cue_send: 1.0,
            cue_tap: CueTap::PostDeckFader,
            side: DeckSide::Center,
        }],
        // `Center` on the side already takes the crossfader out of the path, so the curve is
        // irrelevant here — the default keeps the test config minimal.
        crossfader: CrossfaderConfig::default(),
        master_fx: vec![],
        master_limiter: false,
        master_fader: 0.0,
        cue_fader: 0.0,
        cue: CueMixConfig::default(),
        outputs: vec![],
    }
}

/// Instantiates a list of FX names, in order.
pub fn build_slots(names: &[String]) -> Result<Vec<FxSlot>, MixerError> {
    names.iter().map(|name| build_slot(name)).collect()
}

/// One FX by name, at its defaults.
pub fn build_slot(name: &str) -> Result<FxSlot, MixerError> {
    let kind = FxKind::parse(name).map_err(|_| MixerError::UnknownFx(name.to_owned()))?;
    Ok(FxSlot::new(kind.build(), kind.name()))
}

/// Builds a chain from names.
pub fn build_chain(names: &[String]) -> Result<FxChain, MixerError> {
    Ok(FxChain::from_slots(build_slots(names)?))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::FxKind;

    #[test]
    fn simple_dj_is_the_documented_topology() {
        let cfg = simple_dj();
        assert_eq!(cfg.channels.len(), 2);
        assert_eq!(cfg.outputs.len(), 2);
        assert_eq!(cfg.outputs[0].role, OutputRole::Master);
        assert_eq!(cfg.outputs[1].role, OutputRole::Cue);
        for channel in &cfg.channels {
            assert_eq!(channel.deck_fx, vec!["eq".to_owned(), "filter".to_owned()]);
            // A cue that only sounds when you send to it is not a cue.
            assert!(channel.cue_send > -1.0, "channels must reach the headphones");
            assert_eq!(channel.flow_fader, 0.0, "both decks start at unity");
            assert_eq!(channel.gain, 0.0, "no channel starts with a trim");
        }
        assert_ne!(
            cfg.channels[0].side, cfg.channels[1].side,
            "two decks on one crossfader must be opposite sides"
        );
        assert!(cfg.master_limiter, "the safety stage must be wired by default");
    }

    #[test]
    fn every_named_fx_resolves() {
        let cfg = simple_dj();
        for name in cfg
            .channels
            .iter()
            .flat_map(|c| c.deck_fx.iter().chain(c.flow_fx.iter()))
            .chain(cfg.master_fx.iter())
        {
            build_slot(name).unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }

    #[test]
    fn the_reference_toml_parses_into_simple_dj() {
        // The contract `--print-config` → edit → `--config` depends on: the emitted text and the
        // built-in constructor describe the same mixer. Ids are positional (the mixer renumbers),
        // so they are normalised before the comparison.
        let mut parsed = MixerConfig::from_toml_str(reference_toml())
            .unwrap_or_else(|err| panic!("reference toml does not parse: {err}"));
        for (index, output) in parsed.outputs.iter_mut().enumerate() {
            output.id = index as OutputId;
        }
        let mut expected = simple_dj();
        for (index, output) in expected.outputs.iter_mut().enumerate() {
            output.id = index as OutputId;
        }
        assert_eq!(parsed, expected);
        // And it must actually build: chains resolve, limiter wires.
        assert!(parsed.build_chains().is_ok());
        assert!(parsed.master_limiter);
    }

    #[test]
    fn toml_fields_default_to_the_documented_values() {
        // A minimal channel: everything optional takes the defaults the comments promise.
        let cfg = MixerConfig::from_toml_str(
            r#"
            [[channel]]
            side = "center"
            [[output]]
            name = "only"
            role = "master"
            "#,
        )
        .unwrap();
        let channel = &cfg.channels[0];
        assert_eq!(channel.flow_fader, 0.0, "unity by default");
        assert_eq!(channel.deck_fader, 0.0);
        assert_eq!(channel.cue_send, 0.0, "a default channel does not spam the cue");
        assert_eq!(channel.cue_tap, CueTap::PostDeckFader);
        assert_eq!(channel.gain, 0.0, "the trim starts at 0 dB");
        assert_eq!(cfg.crossfader.place, CrossfaderPlace::Deck, "the crossfader is last by default");
        assert_eq!(cfg.crossfader.curve, CrossfaderCurve::Classic);
        assert_eq!(cfg.crossfader.start, 0.0);
        assert_eq!(cfg.crossfader.cut_width, crate::mixer::DEFAULT_CUT_WIDTH);
        assert_eq!(cfg.cue.blend, -1.0, "a default cue carries the cue and nothing else");
        assert!(channel.flow_fx.is_empty() && channel.deck_fx.is_empty());
        assert_eq!(cfg.outputs[0].channels, (0, 1));
        assert_eq!(cfg.outputs[0].gain, 1.0);
        assert_eq!(cfg.cue_fader, 0.0);
        assert!(!cfg.master_limiter, "the safety stage is opt-in from a file");
        assert_eq!(cfg.master_fader, 0.0);
        // The struct default and the `dj` helper agree with the parsed channel above.
        let default = ChannelConfig::default();
        assert!(default.flow_fx.is_empty() && default.deck_fx.is_empty());
        assert_eq!(default.cue_send, 0.0, "a default channel must not spam the cue");
        assert_eq!(default.side, DeckSide::default());
        let dj = ChannelConfig::dj(["eq"]);
        assert_eq!(dj.deck_fx, vec!["eq".to_owned()]);
        assert!(dj.flow_fx.is_empty());
    }

    #[test]
    fn toml_errors_name_their_location_and_refuse_unknown_keys() {
        // A syntax error carries the line; a typo'd key is refused rather than ignored.
        let err = MixerConfig::from_toml_str("[[channel]]
side = \"spinward\"").unwrap_err();
        assert!(matches!(err, MixerError::Parse(_)));
        assert!(err.message().contains("spinward"), "got: {}", err.message());

        let err = MixerConfig::from_toml_str(
            "[[channel]]\nside = \"left\"\nbass_boost = 12.0\n",
        )
        .unwrap_err();
        assert!(
            err.message().contains("bass_boost"),
            "unknown keys must be named, got: {}",
            err.message()
        );

        // An output without a name has nothing to log against.
        assert!(MixerConfig::from_toml_str("[[output]]\nrole = \"main\"").is_err());
        // Every error variant is displayable.
        for err in [
            MixerError::UnknownFx("x".into()),
            MixerError::BadParam { kind: "eq".into(), name: "low".into() },
            MixerError::Empty,
            MixerError::Output("boom".into()),
        ] {
            assert!(!err.message().is_empty());
        }
    }

    #[test]
    fn an_unknown_fx_is_a_construction_error_not_a_silent_skip() {
        let mut cfg = simple_dj();
        cfg.channels[0].deck_fx.push("reverb".into());
        let err = cfg.build_chains().unwrap_err();
        assert!(matches!(err, MixerError::UnknownFx(_)));
        assert!(err.message().contains("reverb"));
        // The message names what *is* available, because that is the next thing a user asks.
        assert!(err.message().contains(FxKind::Eq.name()));
    }

    #[test]
    fn chains_build_in_the_configured_order() {
        let mut cfg = simple_dj();
        cfg.channels[0].deck_fx = vec!["gain".into(), "eq".into()];
        let chains = cfg.build_chains().unwrap();
        let kinds: Vec<_> = chains[0].1.iter().map(|slot| slot.kind()).collect();
        assert_eq!(kinds, vec!["gain", "eq"]);
        assert_eq!(chains[0].0.len(), Stem::COUNT, "one stream chain per stem");
        assert!(chains[0].0.iter().all(|chain| chain.is_empty()));
    }

    /// The template reaches every stem, and an override replaces it for exactly one.
    #[test]
    fn a_stem_override_replaces_the_template_for_that_stem_only() {
        let mut cfg = simple_dj();
        cfg.channels[0].flow_fx = vec!["gain".into()];
        cfg.channels[0]
            .stem_fx
            .insert(Stem::Vocals, vec!["filter".into(), "gain".into()]);
        let chains = cfg.build_chains().unwrap();
        for stem in Stem::ALL {
            let kinds: Vec<_> = chains[0].0[stem.index()]
                .slots()
                .iter()
                .map(|slot| slot.kind())
                .collect();
            if stem == Stem::Vocals {
                assert_eq!(kinds, vec!["filter", "gain"], "the override wins");
            } else {
                assert_eq!(kinds, vec!["gain"], "{stem} follows the template");
            }
        }
        assert_eq!(cfg.channels[0].chain_names(Stem::Drums), ["gain".to_owned()]);
        assert_eq!(
            cfg.channels[0].chain_names(Stem::Vocals),
            ["filter".to_owned(), "gain".to_owned()]
        );
    }

    #[test]
    fn an_unknown_fx_in_a_stem_override_is_a_construction_error() {
        let mut cfg = simple_dj();
        cfg.channels[0]
            .stem_fx
            .insert(Stem::Vocals, vec!["reverb".into()]);
        let err = cfg.build_chains().unwrap_err();
        assert!(matches!(err, MixerError::UnknownFx(_)), "{err:?}");
        assert!(err.message().contains("reverb"));
    }

    /// The TOML spelling of a per-stem override, which is the point of the field.
    #[test]
    fn a_stem_override_parses_from_toml() {
        let text = r#"
            [[channel]]
            side = "center"
            deck_fx = ["eq"]
            flow_fx = ["gain"]
            [channel.stem_fx]
            vocals = ["filter"]
        "#;
        let cfg = MixerConfig::from_toml_str(text).expect("parses");
        assert_eq!(cfg.channels[0].chain_names(Stem::Vocals), ["filter".to_owned()]);
        assert_eq!(cfg.channels[0].chain_names(Stem::Other), ["gain".to_owned()]);
        assert!(cfg.build_chains().is_ok());
        // An unknown stem name is a parse error, not a silently ignored key.
        assert!(MixerConfig::from_toml_str(
            "[[channel]]\n[channel.stem_fx]\nguitar = [\"eq\"]\n"
        )
        .is_err());
    }

    #[test]
    fn silent_test_config_never_touches_a_device() {
        let cfg = silent_test_channel();
        assert!(cfg.outputs.is_empty());
        assert!(!cfg.master_limiter);
        assert_eq!(cfg.channels.len(), 1);
    }

    #[test]
    fn channel_pair_falls_back_rather_than_going_out_of_range() {
        let cfg = OutputConfig {
            channels: (4, 5),
            ..OutputConfig::master(0, "x")
        };
        assert_eq!(cfg.channel_pair(2), (0, 1), "a 2ch device clamps rather than indexing out");
        assert_eq!(cfg.channel_pair(8), (4, 5));
        assert_eq!(OutputConfig::master(0, "m").channel_pair(1), (0, 1), "1ch writes the sum to ch0");
    }
}
