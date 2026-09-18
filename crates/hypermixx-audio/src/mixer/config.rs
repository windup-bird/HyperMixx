//! [`MixerConfig`]: what a mixer is built from — channels, master/cue chains, outputs.
//!
//! Hard-coded for now, deliberately: [`simple_dj`] is the shape a TOML file will describe later, and
//! keeping the two apart means the config layer can gain a parser without the mixer learning about
//! files. Everything here is plain data (`String` effect names, `f32` fader positions), so it can
//! come from a crate, a socket or a test helper.
//!
//! FX are named by string and resolved through [`FxKind`](crate::fx::FxKind) at build time. A name
//! that is not in the registry is a [`MixerError::UnknownFx`] at construction, not a silent skip:
//! a mixer that quietly dropped the limiter the user configured is worse than one that refuses to
//! start.

use hypermixx_core::FxChainId;

use super::channel::{CueTap, CrossfaderCurve, DeckSide};
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

/// One mixer input.
#[derive(Clone, Debug)]
pub struct ChannelConfig {
    /// FX between the deck and the flow fader. Empty for "no per-stream inserts".
    pub flow_fx: Vec<String>,
    /// FX between the flow fader and the deck fader — the channel's tone controls.
    pub deck_fx: Vec<String>,
    /// Per-stream level, `-1.0 ..= 1.0` (centred = unity).
    pub flow_fader: f32,
    /// Deck level; stays at unity until a track exposes stems.
    pub deck_fader: f32,
    /// Starting crossfade position.
    pub crossfader: f32,
    /// How much of this channel reaches the cue bus: **linear, 0.0 (off) ..= 1.0 (full)**.
    pub cue_send: f32,
    pub cue_tap: CueTap,
    pub side: DeckSide,
    pub crossfader_curve: CrossfaderCurve,
}

impl Default for ChannelConfig {
    /// A muted, centred, effect-free input. Anything a caller builds starts from here so a new field
    /// cannot silently change an existing config's behaviour.
    fn default() -> Self {
        Self {
            flow_fx: Vec::new(),
            deck_fx: Vec::new(),
            flow_fader: 0.0,
            deck_fader: 0.0,
            crossfader: 0.0,
            cue_send: 0.0,
            cue_tap: CueTap::default(),
            side: DeckSide::default(),
            crossfader_curve: CrossfaderCurve::default(),
        }
    }
}

impl ChannelConfig {
    /// Every FX name this channel names, flow chain first. Used for validation and `fx list`.
    pub fn all_fx(&self) -> impl Iterator<Item = &String> {
        self.flow_fx.iter().chain(self.deck_fx.iter())
    }

    /// A channel with `deck_fx` tone controls and no per-stream inserts.
    pub fn dj(deck_fx: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            deck_fx: deck_fx.into_iter().map(Into::into).collect(),
            ..Default::default()
        }
    }

    /// Which chain a command addresses for this channel.
    pub fn chain(&self, chain: SlotPlace) -> &[String] {
        match chain {
            SlotPlace::Flow => &self.flow_fx,
            SlotPlace::Deck => &self.deck_fx,
        }
    }
}

/// Which of a channel's two chains a config entry means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotPlace {
    Flow,
    Deck,
}

/// One destination.
#[derive(Clone, Debug)]
pub struct OutputConfig {
    pub id: OutputId,
    pub name: String,
    /// `(left, right)` device channel indices.
    pub channels: (u16, u16),
    pub role: OutputRole,
    /// Trim applied when the bus is handed over, so two destinations can differ in level without
    /// the mixer knowing about it.
    pub gain: f32,
}

/// Whether an output carries the mix or the cue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputRole {
    #[default]
    Main,
    Headphones,
}

impl OutputConfig {
    pub fn main(id: OutputId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            channels: (0, 1),
            role: OutputRole::Main,
            gain: 1.0,
        }
    }

    pub fn headphones(id: OutputId, name: impl Into<String>) -> Self {
        Self {
            role: OutputRole::Headphones,
            ..Self::main(id, name)
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
#[derive(Clone, Debug, Default)]
pub struct MixerConfig {
    pub channels: Vec<ChannelConfig>,
    /// FX on the summed mix, before the safety limiter.
    pub master_fx: Vec<String>,
    /// Always-on safety limiter, wired separately so it cannot be configured away by a bad list.
    pub master_limiter: bool,
    /// Master trim, `-1.0 ..= 1.0`.
    pub master_fader: f32,
    /// Cue trim.
    pub cue_fader: f32,
    pub outputs: Vec<OutputConfig>,
}

impl MixerConfig {
    /// Builds the chains this config names. Split out so a mixer can report a bad FX name without
    /// having touched a device.
    pub fn build_chains(&self) -> Result<Vec<(Vec<FxSlot>, Vec<FxSlot>)>, MixerError> {
        self.channels
            .iter()
            .map(|channel| {
                Ok((
                    build_slots(&channel.flow_fx)?,
                    build_slots(&channel.deck_fx)?,
                ))
            })
            .collect()
    }

    pub fn master_slots(&self) -> Result<Vec<FxSlot>, MixerError> {
        build_slots(&self.master_fx)
    }

    pub fn output_for(&self, index: usize) -> Option<&OutputConfig> {
        self.outputs.get(index)
    }
}

/// The reference topology: two decks, each with an EQ and a sweep, a limiting master, and main plus
/// headphones.
///
/// This is the function every test and the CLI starts from. If it changes, the acceptance question
/// ("does a cue still work, does the crossfader still isolate a deck?") has a new answer.
pub fn simple_dj() -> MixerConfig {
    let deck_channels = |side: DeckSide, crossfader: f32| ChannelConfig {
        flow_fx: vec![],
        deck_fx: vec!["eq".into(), "filter".into()],
        flow_fader: 0.0,
        deck_fader: 0.0,
        crossfader,
        cue_send: 1.0,
        cue_tap: CueTap::PostDeckFx,
        side,
        crossfader_curve: CrossfaderCurve::EqualPower,
    };
    MixerConfig {
        channels: vec![
            deck_channels(DeckSide::Left, 0.0),
            deck_channels(DeckSide::Right, 0.0),
        ],
        master_fx: vec![],
        master_limiter: true,
        master_fader: 0.0,
        cue_fader: 0.0,
        outputs: vec![
            OutputConfig::main(0, "main"),
            OutputConfig::headphones(1, "headphones"),
        ],
    }
}

/// A single-channel config with no outputs at all: the mixer computes and writes nothing, which is
/// what makes [`Mixer::process`](super::Mixer::process) unit-testable without a sound card.
pub fn silent_test_channel() -> MixerConfig {
    MixerConfig {
        channels: vec![ChannelConfig {
            flow_fx: vec![],
            deck_fx: vec![],
            flow_fader: 0.0,
            deck_fader: 0.0,
            crossfader: 0.0,
            cue_send: 1.0,
            cue_tap: CueTap::PostDeckFader,
            side: DeckSide::Center,
            crossfader_curve: CrossfaderCurve::Linear,
        }],
        master_fx: vec![],
        master_limiter: false,
        master_fader: 0.0,
        cue_fader: 0.0,
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

/// As [`build_slot`], but bypassed — how a safety limiter is armed.
pub fn build_slot_disabled(name: &str) -> Result<FxSlot, MixerError> {
    let kind = FxKind::parse(name).map_err(|_| MixerError::UnknownFx(name.to_owned()))?;
    Ok(FxSlot::disabled(kind.build(), kind.name()))
}

/// Builds a chain from names.
pub fn build_chain(names: &[String]) -> Result<FxChain, MixerError> {
    Ok(FxChain::from_slots(build_slots(names)?))
}

/// The default parameter a caller can read off a fresh instance, for validation in tests.
pub fn defaults_for(name: &str) -> Option<Vec<(String, f32)>> {
    let kind = FxKind::parse(name).ok()?;
    let fx = kind.build();
    Some(
        fx.param_names()
            .iter()
            .map(|n| ((*n).to_owned(), fx.get_param(n).unwrap_or(0.0)))
            .collect(),
    )
}

/// Which chain a command names, resolved against the mixer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainRef {
    Master,
    Cue,
    Channel(usize),
}

impl From<FxChainId> for ChainRef {
    fn from(chain: FxChainId) -> Self {
        match chain {
            FxChainId::Master => ChainRef::Master,
            FxChainId::Deck(deck_id) => ChainRef::Channel(deck_id as usize),
        }
    }
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
        assert_eq!(cfg.outputs[0].role, OutputRole::Main);
        assert_eq!(cfg.outputs[1].role, OutputRole::Headphones);
        for channel in &cfg.channels {
            assert_eq!(channel.deck_fx, vec!["eq".to_owned(), "filter".to_owned()]);
            // A cue that only sounds when you send to it is not a cue.
            assert!(channel.cue_send > -1.0, "channels must reach the headphones");
            assert_eq!(channel.flow_fader, 0.0, "both decks start at unity");
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
        assert!(chains[0].0.is_empty(), "no per-stream inserts configured");
    }

    #[test]
    fn defaults_are_readable_for_validation() {
        for kind in FxKind::ALL {
            let params = defaults_for(kind.name()).unwrap();
            assert_eq!(params.len(), kind.param_names().len(), "{kind}");
            assert!(params.iter().all(|(_, v)| v.is_finite()), "{kind} default");
        }
        assert!(defaults_for("nope").is_none());
    }

    #[test]
    fn a_default_channel_is_muted_and_effect_free() {
        let cfg = ChannelConfig::default();
        assert!(cfg.flow_fx.is_empty() && cfg.deck_fx.is_empty());
        assert_eq!(cfg.cue_send, 0.0, "a default channel must not spam the cue");
        assert_eq!(cfg.side, DeckSide::default());
        let dj = ChannelConfig::dj(["eq"]);
        assert_eq!(dj.deck_fx, vec!["eq".to_owned()]);
        assert!(dj.flow_fx.is_empty());
    }

    #[test]
    fn silent_test_config_never_touches_a_device() {
        let cfg = silent_test_channel();
        assert!(cfg.outputs.is_empty());
        assert!(!cfg.master_limiter);
        assert_eq!(cfg.channels.len(), 1);
    }

    #[test]
    fn chain_refs_match_the_protocol() {
        assert_eq!(ChainRef::from(FxChainId::Master), ChainRef::Master);
        assert_eq!(
            ChainRef::from(FxChainId::Deck(1)),
            ChainRef::Channel(1)
        );
    }

    #[test]
    fn channel_pair_falls_back_rather_than_going_out_of_range() {
        let cfg = OutputConfig {
            channels: (4, 5),
            ..OutputConfig::main(0, "x")
        };
        assert_eq!(cfg.channel_pair(2), (0, 1));
        assert_eq!(cfg.channel_pair(8), (4, 5));
        assert_eq!(OutputConfig::main(0, "m").channel_pair(1), (0, 1));
    }

    #[test]
    fn errors_are_displayable_and_specific() {
        for err in [
            MixerError::UnknownFx("x".into()),
            MixerError::BadParam {
                kind: "eq".into(),
                name: "low".into(),
            },
            MixerError::Empty,
            MixerError::Output("boom".into()),
        ] {
            assert!(!err.message().is_empty());
        }
    }
}
