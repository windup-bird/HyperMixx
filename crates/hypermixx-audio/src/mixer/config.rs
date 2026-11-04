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

use hypermixx_core::{OutputRole, Stem, CHANNELS};

use super::channel::{CrossfaderCurve, CrossfaderPlace, CueTap, DeckSide};
use super::output::OutputId;
use crate::fx::sample::FilterMode;
use crate::fx::{FxChain, FxError, FxKind, FxSlot, FLOW_ALL};
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

/// One channel's built chains: one chain per flow (the same fixed prefix plus the `flowfx` slots),
/// then the post-merge chain (knob bank, then pad bank).
pub type BuiltChains = (Vec<FxChain>, FxChain);

/// One mixer input.
///
/// The stage order inside a channel is fixed (it *is* the design): each flow gets the same tone
/// prefix — `eq`, then the sweep — followed by the `flowfx` slots, and the flows are summed; the
/// post-merge chain holds the knob bank and then the pad bank; the channel fader sits after both.
/// Configuration decides *what* is in each stage, never the order.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChannelConfig {
    /// Which sweep the fixed `filter` stage builds. Both kinds expose the same `value`/`resonance`
    /// parameters, so the stage can also be swapped at run time with `fx kind filter moog`.
    pub filter: FilterMode,
    /// Per-flow inserts, as a **template**: every flow gets its own chain built from this list, so
    /// four streams get four independent instances (separate filter state, not one shared filter).
    ///
    /// These run *after* the tone prefix (`eq`, `filter`) and *before* the flows are summed. Each
    /// slot can name the flows it is routed to (`flows = ["vocals"]`); omitted means every flow.
    pub flow_fx: Vec<FlowSlotConfig>,
    /// Post-merge knob bank: continuous effects (a sweep, an echo) addressed by the first knob.
    pub knob_fx: Vec<String>,
    /// Post-merge pad bank: momentary effects (a gate, a roll), after the knob bank.
    pub pad_fx: Vec<String>,
    /// Per-stream level, `-1.0 ..= 1.0` (centred = unity, the top of the travel is 0 dB). This is a
    /// *stem's* level; the channel fader is [`ChannelConfig::deck_fader`].
    pub flow_fader: f32,
    /// The channel fader's starting level, `-1.0 ..= 1.0`. The name is historical; the fader sits
    /// after the post-merge chain.
    pub deck_fader: f32,
    /// Channel trim, `-1.0 ..= 1.0`: `0.0` is 0 dB, `1.0` is +6 dB (the only stage that boosts),
    /// `-1.0` is silence. It sits after the flow sum and before the post-merge chain.
    pub gain: f32,
    /// How much of this channel reaches the cue bus: **linear, 0.0 (off) ..= 1.0 (full)**.
    pub cue_send: f32,
    /// Which side of the channel fader the cue send taps: `pre` (default) keeps a channel cueable
    /// with its fader down; `post` makes the fader silence the monitor too.
    pub cue: CueTap,
    pub side: DeckSide,
}

impl Default for ChannelConfig {
    /// A muted, centred, effect-free input. Anything a caller builds starts from here so a new field
    /// cannot silently change an existing config's behaviour.
    fn default() -> Self {
        Self {
            filter: FilterMode::Linear,
            flow_fx: Vec::new(),
            knob_fx: Vec::new(),
            pad_fx: Vec::new(),
            flow_fader: 0.0,
            deck_fader: 0.0,
            gain: 0.0,
            cue_send: 0.0,
            cue: CueTap::default(),
            side: DeckSide::default(),
        }
    }
}

/// One entry of the `flowfx` bank: the effect and which flows it is routed to.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FlowSlotConfig {
    /// The effect kind (a name from `fx help`).
    pub kind: String,
    /// The flows this slot is routed to, by stem name. Omitted = every flow; `[]` = no flow (the
    /// slot is mounted but processes nothing).
    pub flows: Option<Vec<Stem>>,
}

impl Default for FlowSlotConfig {
    fn default() -> Self {
        Self {
            kind: String::new(),
            flows: None,
        }
    }
}

impl FlowSlotConfig {
    /// A slot routed to every flow.
    pub fn all(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            flows: None,
        }
    }

    /// A slot routed to exactly `flows`.
    pub fn only(kind: impl Into<String>, flows: impl IntoIterator<Item = Stem>) -> Self {
        Self {
            kind: kind.into(),
            flows: Some(flows.into_iter().collect()),
        }
    }

    /// The flow mask the mixer stores on the slot.
    pub fn mask(&self) -> u8 {
        match &self.flows {
            None => FLOW_ALL,
            Some(flows) => flows
                .iter()
                .fold(0u8, |mask, stem| mask | (1u8 << (stem.index() & 7))),
        }
    }
}

impl ChannelConfig {
    /// Every FX name this channel names — every bank — for validation and `fx help`.
    pub fn all_fx(&self) -> impl Iterator<Item = &String> {
        self.flow_fx
            .iter()
            .map(|slot| &slot.kind)
            .chain(self.knob_fx.iter())
            .chain(self.pad_fx.iter())
    }

    /// A channel with a post-merge knob bank and no per-flow inserts.
    pub fn dj(knob_fx: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            knob_fx: knob_fx.into_iter().map(Into::into).collect(),
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
    /// Which physical device to bind to: a **name substring** (case-insensitive, first match wins)
    /// or an **index** into the host's output devices. Omitted = the system default device.
    ///
    /// A substring rather than an id because the names are the only thing every platform agrees on
    /// (ALSA, PipeWire, WASAPI and future Android backends all have human names, and hardware ids
    /// change between machines); an index is there for the moment, when you can see the list.
    #[serde(default)]
    pub device: Option<String>,
    /// `(left, right)` device channel indices.
    #[serde(default = "default_channels")]
    pub channels: (u16, u16),
    pub role: OutputRole,
    /// The sample rates this destination is allowed to run at. Defaults to the engine's rate, so a
    /// device that cannot take it leaves the output **disabled** (with a warning) rather than
    /// silently playing at the wrong speed.
    #[serde(default)]
    pub sample_rates: Vec<u32>,
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

impl Default for OutputConfig {
    /// The plain stereo default: channels (0,1), main role, unity gain. `name` still has to come
    /// from the config — an anonymous destination cannot be named in a log line.
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            device: None,
            channels: (0, 1),
            role: OutputRole::Master,
            sample_rates: Vec::new(),
            gain: 1.0,
        }
    }
}

impl OutputConfig {
    /// A stereo master destination on the system default device.
    pub fn master(id: OutputId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            ..Self::default()
        }
    }

    /// A cue (headphone) destination.
    pub fn cue(id: OutputId, name: impl Into<String>) -> Self {
        Self {
            role: OutputRole::Cue,
            ..Self::master(id, name)
        }
    }

    /// The trim to apply, or unity when the config is nonsense.
    pub fn gain_or_unity(&self) -> f32 {
        if self.gain.is_finite() && self.gain >= 0.0 {
            self.gain
        } else {
            1.0
        }
    }

    /// Whether this destination may run at the engine's rate. An empty `sample_rates` means
    /// "whatever the engine runs at", which is why the engine rate is the argument: comparing
    /// against a hard-coded 44.1 kHz disabled every output on a 48 kHz engine.
    pub fn allows_engine_rate(&self, engine_rate: u32) -> bool {
        self.sample_rates.is_empty() || self.sample_rates.contains(&engine_rate)
    }

    /// How many device channels this destination needs: at least stereo, and enough to hold both
    /// configured indices. A cue on `(2, 3)` therefore opens a 4-channel stream, while `(0, 1)`
    /// still opens stereo on a stereo-only device.
    pub fn wanted_device_channels(&self) -> usize {
        (self.channels.0.max(self.channels.1) as usize + 1).max(CHANNELS)
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
        self.channels.iter().map(build_channel_chains).collect()
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
        let file: MixerFile =
            toml::from_str(text).map_err(|err| MixerError::Parse(err.to_string()))?;
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
        // The tone prefix is part of the topology: `eq` is always there and `filter` is a fixed
        // stage whose kind is the only choice. The reference leaves the two banks empty.
        filter: FilterMode::Linear,
        flow_fx: vec![],
        knob_fx: vec![],
        pad_fx: vec![],
        flow_fader: 0.0,
        deck_fader: 0.0,
        gain: 0.0,
        cue_send: 1.0,
        cue: CueTap::default(),
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
    // `--print-config` and `profile/mixer.toml` are the same bytes.
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../profile/mixer.toml"
    ))
}

/// A single-channel config with no outputs at all: the mixer computes and writes nothing, which is
/// what makes [`Mixer::process`](super::Mixer::process) unit-testable without a sound card.
pub fn silent_test_channel() -> MixerConfig {
    MixerConfig {
        channels: vec![ChannelConfig {
            flow_fx: vec![],
            knob_fx: vec![],
            pad_fx: vec![],
            flow_fader: 0.0,
            deck_fader: 0.0,
            gain: 0.0,
            cue_send: 1.0,
            cue: CueTap::default(),
            side: DeckSide::Center,
            ..Default::default()
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

/// One FX by name, at its defaults. Its role is the kind's name — a bank slot gets a numbered role
/// from [`build_channel_chains`] instead.
pub fn build_slot(name: &str) -> Result<FxSlot, MixerError> {
    Ok(slot_with_role(name, None)?)
}

/// One FX by name with an explicit role (the slot's stable identity; see [`FxSlot`]).
pub fn slot_with_role(name: &str, role: Option<&str>) -> Result<FxSlot, MixerError> {
    let kind = FxKind::parse(name).map_err(|_| MixerError::UnknownFx(name.to_owned()))?;
    Ok(match role {
        Some(role) => FxSlot::with_role(kind.build(), kind.name(), role),
        None => FxSlot::new(kind.build(), kind.name()),
    })
}

/// Every chain one channel owns: the per-flow chains (the same tone prefix and `flowfx` slots for
/// each flow) and the post-merge chain (knob bank, then pad bank).
fn build_channel_chains(cfg: &ChannelConfig) -> Result<BuiltChains, MixerError> {
    let flow = (0..Stem::COUNT)
        .map(|_| {
            // `eq` first, then the sweep — part of the path, not of any bank.
            let mut slots = vec![
                slot_with_role("eq", Some("eq"))?,
                slot_with_role(cfg.filter.name(), Some("filter"))?,
            ];
            for (index, slot_cfg) in cfg.flow_fx.iter().enumerate() {
                let mut slot =
                    slot_with_role(&slot_cfg.kind, Some(&format!("flowfx{}", index + 1)))?;
                slot.set_flows(slot_cfg.mask());
                slots.push(slot);
            }
            Ok(FxChain::from_slots(slots))
        })
        .collect::<Result<Vec<_>, MixerError>>()?;

    let mut post = Vec::with_capacity(cfg.knob_fx.len() + cfg.pad_fx.len());
    for (index, name) in cfg.knob_fx.iter().enumerate() {
        post.push(slot_with_role(name, Some(&format!("knobfx{}", index + 1)))?);
    }
    for (index, name) in cfg.pad_fx.iter().enumerate() {
        post.push(slot_with_role(name, Some(&format!("padfx{}", index + 1)))?);
    }
    Ok((flow, FxChain::from_slots(post)))
}

/// A chain whose slots carry numbered roles (`masterfx1`, `masterfx2`, …), for the master bus.
pub fn build_chain_with_roles(names: &[String], prefix: &str) -> Result<FxChain, MixerError> {
    let slots = names
        .iter()
        .enumerate()
        .map(|(index, name)| slot_with_role(name, Some(&format!("{prefix}{}", index + 1))))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FxChain::from_slots(slots))
}

/// Builds a chain from names.
pub fn build_chain(names: &[String]) -> Result<FxChain, MixerError> {
    Ok(FxChain::from_slots(build_slots(names)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_dj_is_the_documented_topology() {
        let cfg = simple_dj();
        assert_eq!(cfg.channels.len(), 2);
        assert_eq!(cfg.outputs.len(), 2);
        assert_eq!(cfg.outputs[0].role, OutputRole::Master);
        assert_eq!(cfg.outputs[1].role, OutputRole::Cue);
        for channel in &cfg.channels {
            // The tone prefix is part of the path; the banks are the configurable part.
            assert_eq!(channel.filter, FilterMode::Linear);
            assert!(channel.flow_fx.is_empty() && channel.knob_fx.is_empty() && channel.pad_fx.is_empty());
            assert!(
                channel.cue_send > -1.0,
                "channels must reach the headphones"
            );
            assert_eq!(channel.flow_fader, 0.0, "both decks start at unity");
            assert_eq!(channel.gain, 0.0, "no channel starts with a trim");
            assert_eq!(channel.cue, CueTap::Pre, "the reference cue is pre-fader");
        }
        assert_ne!(
            cfg.channels[0].side, cfg.channels[1].side,
            "two decks on one crossfader must be opposite sides"
        );
        assert!(
            cfg.master_limiter,
            "the safety stage must be wired by default"
        );
    }

    #[test]
    fn every_named_fx_resolves() {
        let cfg = simple_dj();
        for name in cfg
            .channels
            .iter()
            .flat_map(|c| c.all_fx())
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
            // `device` and `channels` are *machine* facts: the F9 output chooser writes them into
            // this very file, so a rig with its cue on 2,3 must not break this shape contract.
            output.device = None;
            output.channels = (0, 1);
        }
        let mut expected = simple_dj();
        for (index, output) in expected.outputs.iter_mut().enumerate() {
            output.id = index as OutputId;
            output.device = None;
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
        assert_eq!(channel.filter, FilterMode::Linear, "the clean sweep by default");
        assert!(channel.flow_fx.is_empty() && channel.knob_fx.is_empty() && channel.pad_fx.is_empty());
        assert_eq!(channel.flow_fader, 0.0, "unity by default");
        assert_eq!(channel.deck_fader, 0.0);
        assert_eq!(
            channel.cue_send, 0.0,
            "a default channel does not spam the cue"
        );
        assert_eq!(
            channel.cue,
            CueTap::Pre,
            "the cue taps before the fader by default, so the fader never silences the monitor"
        );
        assert_eq!(channel.gain, 0.0, "the trim starts at 0 dB");
        assert_eq!(
            cfg.crossfader.place,
            CrossfaderPlace::Deck,
            "the crossfader is last by default"
        );
        assert_eq!(cfg.crossfader.curve, CrossfaderCurve::Classic);
        assert_eq!(cfg.crossfader.start, 0.0);
        assert_eq!(cfg.crossfader.cut_width, crate::mixer::DEFAULT_CUT_WIDTH);
        assert_eq!(
            cfg.cue.blend, -1.0,
            "a default cue carries the cue and nothing else"
        );
        assert_eq!(cfg.outputs[0].channels, (0, 1));
        assert_eq!(cfg.outputs[0].gain, 1.0);
        assert_eq!(cfg.cue_fader, 0.0);
        assert!(
            !cfg.master_limiter,
            "the safety stage is opt-in from a file"
        );
        assert_eq!(cfg.master_fader, 0.0);
        // The struct default and the `dj` helper agree with the parsed channel above.
        let default = ChannelConfig::default();
        assert!(default.flow_fx.is_empty() && default.knob_fx.is_empty() && default.pad_fx.is_empty());
        assert_eq!(
            default.cue_send, 0.0,
            "a default channel must not spam the cue"
        );
        assert_eq!(default.side, DeckSide::default());
        let dj = ChannelConfig::dj(["eq"]);
        assert_eq!(dj.knob_fx, vec!["eq".to_owned()]);
        assert!(dj.flow_fx.is_empty());
    }

    #[test]
    fn toml_errors_name_their_location_and_refuse_unknown_keys() {
        // A syntax error carries the line; a typo'd key is refused rather than ignored.
        let err = MixerConfig::from_toml_str(
            "[[channel]]
side = \"spinward\"",
        )
        .unwrap_err();
        assert!(matches!(err, MixerError::Parse(_)));
        assert!(err.message().contains("spinward"), "got: {}", err.message());

        let err = MixerConfig::from_toml_str("[[channel]]\nside = \"left\"\nbass_boost = 12.0\n")
            .unwrap_err();
        assert!(
            err.message().contains("bass_boost"),
            "unknown keys must be named, got: {}",
            err.message()
        );

        // The pre-bank spelling is gone: `stem_fx` and `chain` are refused by name rather than
        // silently ignored, so an old topology file cannot half-load.
        for gone in ["stem_fx", "chain"] {
            let text = format!("[[channel]]\n{gone} = [\"eq\"]\n");
            let err = MixerConfig::from_toml_str(&text).unwrap_err();
            assert!(
                err.message().contains(gone),
                "`{gone}` should be named in the error, got: {}",
                err.message()
            );
        }

        // An output without a name has nothing to log against.
        assert!(MixerConfig::from_toml_str("[[output]]\nrole = \"main\"").is_err());
        // Every error variant is displayable.
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

    #[test]
    fn an_unknown_fx_is_a_construction_error_not_a_silent_skip() {
        let mut cfg = simple_dj();
        cfg.channels[0].knob_fx.push("shimmer".into());
        let err = cfg.build_chains().unwrap_err();
        assert!(matches!(err, MixerError::UnknownFx(_)));
        assert!(err.message().contains("shimmer"));
        // The message names what *is* available, because that is the next thing a user asks.
        assert!(err.message().contains(FxKind::Eq.name()));
    }

    /// The stage order and the banks: `eq`, `filter`, then the flow slots per flow; the post-merge
    /// chain is the knob bank followed by the pad bank. Roles are numbered per bank.
    #[test]
    fn the_banks_build_in_the_documented_order_with_numbered_roles() {
        let mut cfg = simple_dj();
        cfg.channels[0].flow_fx = vec![FlowSlotConfig::all("gain")];
        cfg.channels[0].knob_fx = vec!["echo".into(), "filter".into()];
        cfg.channels[0].pad_fx = vec!["reverb".into()];
        let chains = cfg.build_chains().unwrap();
        let (flow, post) = &chains[0];
        assert_eq!(flow.len(), Stem::COUNT, "one tone prefix per flow");

        let kinds = |chain: &FxChain| -> Vec<(&'static str, String)> {
            chain
                .slots()
                .iter()
                .map(|slot| (slot.kind(), slot.role().to_owned()))
                .collect()
        };
        // Every flow carries the same prefix and the same `flowfx` slots, each its own instance.
        for chain in flow {
            let got = kinds(chain);
            assert_eq!(
                got,
                vec![
                    ("eq", "eq".to_owned()),
                    ("filter", "filter".to_owned()),
                    ("gain", "flowfx1".to_owned()),
                ]
            );
        }
        assert!(!std::ptr::eq(&flow[0], &flow[1]), "flows must not share a chain");
        assert_eq!(
            kinds(post),
            vec![
                ("echo", "knobfx1".to_owned()),
                ("filter", "knobfx2".to_owned()),
                ("reverb", "padfx1".to_owned()),
            ]
        );
    }

    /// `filter` chooses the kind of the fixed stage; the slot's role stays `filter`, which is what a
    /// mapped knob and `fx kind` rely on.
    #[test]
    fn the_filter_kind_configures_the_fixed_stage() {
        let cfg = MixerConfig::from_toml_str(
            r#"
            [[channel]]
            filter = "moog"
            "#,
        )
        .expect("parses");
        assert_eq!(cfg.channels[0].filter, FilterMode::Moog);
        let chains = cfg.build_chains().unwrap();
        let slot = &chains[0].0[0].slots()[1];
        assert_eq!(slot.kind(), "moog");
        assert_eq!(slot.role(), "filter");
    }

    /// The `flowfx` bank is a template: every flow gets its own instance of every slot.
    #[test]
    fn a_flowfx_slot_exists_once_per_flow_but_is_one_control() {
        let text = r#"
            [[channel]]
            knob_fx = ["filter"]
            pad_fx = []

            [[channel.flow_fx]]
            kind = "filter"

            [[channel.flow_fx]]
            kind = "echo"
            flows = ["vocals", "drums"]
        "#;
        let cfg = MixerConfig::from_toml_str(text).expect("parses");
        let chains = cfg.build_chains().unwrap();
        assert_eq!(chains[0].0.len(), 4);
        for chain in &chains[0].0 {
            assert_eq!(chain.len(), 2 + 2, "eq, filter, then two flow slots");
            assert_eq!(chain.slots()[2].role(), "flowfx1");
            assert_eq!(chain.slots()[3].role(), "flowfx2");
            // No `flows` on the first slot = every flow; the second is routed to vocals+drums.
            assert_eq!(
                chain.slots()[2].flows(),
                crate::fx::FLOW_ALL,
                "an omitted whitelist routes every flow"
            );
            let mask = (1u8 << (Stem::Vocals.index() & 7)) | (1u8 << (Stem::Drums.index() & 7));
            assert_eq!(chain.slots()[3].flows(), mask);
        }
        // An explicit empty whitelist is a mounted slot that processes nothing.
        let cfg = MixerConfig::from_toml_str(
            r#"
            [[channel]]
            [[channel.flow_fx]]
            kind = "echo"
            flows = []
        "#,
        )
        .expect("parses");
        let chains = cfg.build_chains().unwrap();
        assert_eq!(chains[0].0[0].slots()[2].flows(), 0);

        // And an unknown name in any bank is a construction error.
        let mut cfg = cfg;
        cfg.channels[0].flow_fx.push(FlowSlotConfig::all("shimmer"));
        assert!(matches!(
            cfg.build_chains(),
            Err(MixerError::UnknownFx(_))
        ));
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
        assert_eq!(
            cfg.channel_pair(2),
            (0, 1),
            "a 2ch device clamps rather than indexing out"
        );
        assert_eq!(cfg.channel_pair(8), (4, 5));
        assert_eq!(
            OutputConfig::master(0, "m").channel_pair(1),
            (0, 1),
            "1ch writes the sum to ch0"
        );
    }

    #[test]
    fn an_output_opens_enough_device_channels_for_its_pair() {
        assert_eq!(
            OutputConfig::master(0, "stereo").wanted_device_channels(),
            2
        );
        let cue = OutputConfig {
            channels: (2, 3),
            ..OutputConfig::cue(1, "cue")
        };
        assert_eq!(cue.wanted_device_channels(), 4);
        // Reversed or high indices still ask for the channel they name, not one past it.
        let high = OutputConfig {
            channels: (5, 4),
            ..OutputConfig::master(0, "high")
        };
        assert_eq!(high.wanted_device_channels(), 6);
    }
}
