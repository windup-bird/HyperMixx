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

use hypermixx_core::{AutogainSettings, OutputRole, Stem, CHANNELS};

use super::channel::{CrossfaderCurve, CueTap, DeckSide};
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
    pub side: DeckSide,
    /// Which controller surface drives this deck (`<controller>.<face>[.mirror]`). Routing
    /// metadata for the front-end; the engine never looks at it. `None` means no controller owns
    /// this deck (keyboard / CLI only).
    pub source: Option<String>,
}

impl Default for ChannelConfig {
    /// A muted, centred, effect-free input. Anything a caller builds starts from here so a new field
    /// cannot silently change an existing config's behaviour.
    fn default() -> Self {
        Self {
            flow_fader: 0.0,
            deck_fader: 0.0,
            gain: 0.0,
            cue_send: 0.0,
            side: DeckSide::default(),
            source: None,
        }
    }
}

/// One entry of the `flowfx` bank: the effect and which flows it is routed to.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FlowSlotConfig {
    /// The effect kind (a name from `fx help`).
    pub kind: String,
    /// The flows this slot is routed to, by stem name. Omitted = every flow; `[]` = no flow (the
    /// slot is mounted but processes nothing).
    pub flows: Option<Vec<Stem>>,
}

/// The bit a flow index occupies in a slot's flow mask. One definition, shared by the config, the
/// command surface and status reporting so the three cannot disagree about the encoding.
pub(crate) fn flow_bit(index: usize) -> u8 {
    1u8 << (index & 7)
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
                .fold(0u8, |mask, stem| mask | flow_bit(stem.index())),
        }
    }
}

/// The channel EQ's two crossovers. The middle bell sits at their geometric centre, so the three
/// bands stay log-evenly spaced when a rig moves the shelf frequencies.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EqConfig {
    pub lower_hz: f32,
    pub higher_hz: f32,
}

impl Default for EqConfig {
    fn default() -> Self {
        Self {
            lower_hz: 250.0,
            higher_hz: 2_000.0,
        }
    }
}

impl EqConfig {
    /// `(low shelf, middle bell, high shelf)` in Hz. Nonsense or an inverted pair falls back to the
    /// shipped frequencies rather than asking the filter designer to solve an impossible equation.
    pub fn frequencies(&self) -> (f32, f32, f32) {
        let sane = |value: f32, fallback: f32| {
            if value.is_finite() && value > 0.0 {
                value.clamp(20.0, 20_000.0)
            } else {
                fallback
            }
        };
        let mut low = sane(self.lower_hz, 250.0);
        let mut high = sane(self.higher_hz, 2_000.0);
        if high <= low * 1.01 {
            low = 250.0;
            high = 2_000.0;
        }
        let mid = (low * high).sqrt();
        (low, mid, high)
    }
}

impl MixerConfig {
    /// Resizes the deck list to `count`, keeping what the config already describes and appending
    /// defaults for the rest.
    ///
    /// The front-end owns the deck count (`--4-decks`); a config's `[[multideck.deck]]` rows only say
    /// which controller face drives which deck. A file that maps two faces therefore still runs four
    /// decks: the extra two keep unity faders and a full cue send, take crossing sides of the
    /// crossfader, and simply have no controller.
    pub fn set_channel_count(&mut self, count: usize) {
        while self.channels.len() < count {
            let index = self.channels.len();
            self.channels.push(ChannelConfig {
                cue_send: 1.0,
                side: if index.is_multiple_of(2) {
                    DeckSide::Left
                } else {
                    DeckSide::Right
                },
                ..ChannelConfig::default()
            });
        }
        self.channels.truncate(count);
    }

    /// Every FX name the chain names — every bank — for validation and `fx help`.
    pub fn all_fx(&self) -> impl Iterator<Item = &String> {
        self.flow_fx
            .iter()
            .map(|slot| &slot.kind)
            .chain(self.knob_fx.iter())
            .chain(self.pad_fx.iter())
            .chain(self.master_fx.iter())
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
    pub curve: CrossfaderCurve,
    /// Fraction of the travel the `cut` curve drops over, at each end.
    pub cut_width: f32,
    /// Starting position, `-1.0` hard left .. `1.0` hard right.
    pub start: f32,
}

impl Default for CrossfaderConfig {
    fn default() -> Self {
        Self {
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
    /// `-1.0` = cue only, `0.0` = both at the curve's centre, `1.0` = master only. The file
    /// spells this `[mixer.fader] cue_blend`.
    pub blend: f32,
    pub curve: CrossfaderCurve,
    /// Fraction of the blend the `cut` curve drops over, at each end.
    pub cut_width: f32,
}

impl Default for CueMixConfig {
    fn default() -> Self {
        Self {
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
#[derive(Clone, Debug, PartialEq)]
pub struct MixerConfig {
    pub channels: Vec<ChannelConfig>,
    /// The shared crossfader (one physical control, every channel applies it on its own side).
    pub crossfader: CrossfaderConfig,
    /// Per-flow inserts shared by every channel. A **template**: every flow gets its own chain
    /// built from this list, so four streams get four independent instances.
    pub flow_fx: Vec<FlowSlotConfig>,
    /// Post-merge knob bank shared by every channel.
    pub knob_fx: Vec<String>,
    /// Post-merge pad bank shared by every channel.
    pub pad_fx: Vec<String>,
    /// FX on the summed mix, before the safety limiter.
    pub master_fx: Vec<String>,
    /// Always-on safety limiter, wired separately so it cannot be configured away by a bad list.
    pub master_limiter: bool,
    /// The limiter's ceiling, dBFS. Converted to a linear amplitude when the slot is built; the
    /// limiter clamps it into its own useful range (−40 dBFS .. +12 dBFS).
    pub master_limiter_ceiling_db: f32,
    /// How a `cue` output blends the cue sends with the master mix.
    pub cue: CueMixConfig,
    pub outputs: Vec<OutputConfig>,
    /// Auto-gain: match each loaded track's measured loudness to a target. A mixer setting, so
    /// `mixer reload` re-applies it while `live reload` leaves it alone.
    pub autogain: AutogainSettings,
    /// Which sweep the fixed `filter` stage builds, for every channel (`[mixer.fx] filter`).
    pub filter: FilterMode,
    /// Which side of the channel fader the cue send taps, for every channel (`[mixer.fader]`). `pre` (default) keeps a channel cueable with its fader down; `post` makes the
    /// fader silence the monitor too.
    pub fader_location: CueTap,
    /// The fixed three-band tone stage's crossovers (`[mixer.eq]`).
    pub eq: EqConfig,
}

impl Default for MixerConfig {
    fn default() -> Self {
        Self {
            channels: Vec::new(),
            crossfader: CrossfaderConfig::default(),
            flow_fx: Vec::new(),
            knob_fx: Vec::new(),
            pad_fx: Vec::new(),
            master_fx: Vec::new(),
            // The safety stage is on by default everywhere: in a parsed config, in the
            // reference topology (`simple_dj`) and in this constructor.
            master_limiter: true,
            master_limiter_ceiling_db: -1.0,
            cue: CueMixConfig::default(),
            outputs: Vec::new(),
            autogain: AutogainSettings::default(),
            filter: FilterMode::Linear,
            fader_location: CueTap::default(),
            eq: EqConfig::default(),
        }
    }
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
            .map(|channel| build_channel_chains(self, channel))
            .collect()
    }

    /// Parses a mixer topology from TOML text.
    ///
    /// This is the parser half of the "config is plain data" rule: the file schema is spelled out
    /// by [`MixerFile`] right below — a small mirror of [`MixerConfig`] whose only job is naming
    /// (`[[mixer.fx]]`, `[[multideck.deck]]`), so the Rust type and the file format can each keep
    /// the names that suit them. File IO stays with the caller — a front-end decides where
    /// topologies come from, the engine only reads the text.
    ///
    /// A config that parses but names an unknown effect still succeeds here; that is a
    /// *construction* error ([`MixerError::UnknownFx`] from `build_chains`), surfaced when a mixer
    /// is built rather than when the text is read, because that is where the registry is consulted.
    pub fn from_toml_str(text: &str) -> Result<Self, MixerError> {
        let value: toml::Value =
            toml::from_str(text).map_err(|err| MixerError::Parse(err.to_string()))?;
        let mixer_section = value.get("mixer").cloned();
        let deck_section = value
            .get("multideck")
            .and_then(|table| table.get("deck"))
            .cloned();

        if mixer_section
            .as_ref()
            .and_then(|section| section.get("channel"))
            .is_some()
        {
            return Err(MixerError::Parse(
                "[[mixer.channel]] was removed; put each deck's fields in [[multideck.deck]]"
                    .to_owned(),
            ));
        }

        // A config with only front-end sections is legal: it takes the shipped reference.
        if mixer_section.is_none()
            && deck_section.is_none()
            && ["live", "preference", "controller"]
                .iter()
                .any(|key| value.get(key).is_some())
        {
            return Ok(simple_dj());
        }

        // The old root-level topology (`[[channel]]`, `[chain]`, `[master]`) is gone. Say so
        // instead of silently building an empty mixer from a file that has no `[mixer]` table.
        if mixer_section.is_none() && deck_section.is_none() {
            if let Some(key) = ["channel", "chain", "master", "autogain", "output"]
                .into_iter()
                .find(|key| value.get(*key).is_some())
            {
                return Err(MixerError::Parse(format!(
                    "root-level `{key}` was removed; put the mixer tables under `[mixer]`"
                )));
            }
        }

        let section = mixer_section.unwrap_or_else(|| toml::Value::Table(Default::default()));
        let file: MixerFile = section
            .try_into()
            .map_err(|err| MixerError::Parse(err.to_string()))?;
        let mut cfg: MixerConfig = file.into();
        if let Some(decks) = deck_section {
            let decks: Vec<DeckFile> = decks
                .try_into()
                .map_err(|err| MixerError::Parse(format!("[multideck]: {err}")))?;
            cfg.channels = decks.into_iter().map(ChannelConfig::from).collect();
        }
        Ok(cfg)
    }
}

/// One `[[multideck.deck]]`: the controller routing plus the per-deck audio parameters. Everything
/// shared by every deck lives under `[mixer]`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct DeckFile {
    /// `<controller>.<face>[.mirror]`. Omit for a deck no controller owns.
    source: Option<String>,
    /// Per-deck audio parameters. Everything that is the *same* on every deck lives in
    /// `[mixer.fx]` / `[mixer.fader]`; these are the ones a DJ sets per deck.
    side: DeckSide,
    cue_send: f32,
    flow_fader: f32,
    deck_fader: f32,
    gain: f32,
}

impl From<DeckFile> for ChannelConfig {
    fn from(deck: DeckFile) -> Self {
        Self {
            flow_fader: deck.flow_fader,
            deck_fader: deck.deck_fader,
            gain: deck.gain,
            cue_send: deck.cue_send,
            side: deck.side,
            source: deck.source,
        }
    }
}

/// The TOML schema for the `[mixer]` section: the bus shape (`[crossfader]`, `[fx]`, `[eq]`,
/// `[limiter]`, `[fader]`, `[cue]`, `[autogain]`) and any number of `[[mixer.output]]`s. The deck list
/// lives in the sibling `[[multideck.deck]]` array. Every field defaults.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct MixerFile {
    crossfader: CrossfaderConfig,
    /// `[mixer.fx]`: the four banks and the fixed sweep's kind.
    fx: FxFile,
    /// `[mixer.eq]`: the fixed tone stage's two crossovers.
    eq: EqConfig,
    /// `[mixer.limiter]`: the safety stage.
    limiter: LimiterFile,
    /// `[mixer.fader]`: cue tap side, cue blend and the cue blend curve.
    fader: FaderFile,
    /// `[mixer.cue]`: the cue bus shape. The trim is a runtime value, not a config value.
    cue: CueFile,
    output: Vec<OutputConfig>,
    /// `[mixer.autogain]`.
    autogain: AutogainSettings,
}

/// The `[mixer.fx]` table: the four banks plus the fixed sweep's kind. Their order is fixed by the
/// signal path; the lists choose what sits in each stage.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct FxFile {
    master_fx: Vec<String>,
    flow_fx: Vec<FlowSlotConfig>,
    knob_fx: Vec<String>,
    pad_fx: Vec<String>,
    filter: FilterMode,
}

impl Default for FxFile {
    fn default() -> Self {
        Self {
            master_fx: Vec::new(),
            flow_fx: Vec::new(),
            knob_fx: Vec::new(),
            pad_fx: Vec::new(),
            filter: FilterMode::Linear,
        }
    }
}

/// The `[mixer.limiter]` table. `limiter` is also accepted as `enabled`, because a section named
/// "limiter" repeating the word is easy to get wrong.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct LimiterFile {
    #[serde(alias = "enabled")]
    limiter: bool,
    ceiling_db: f32,
}

impl Default for LimiterFile {
    fn default() -> Self {
        Self {
            limiter: true,
            ceiling_db: -1.0,
        }
    }
}

/// The `[mixer.fader]` table. The cue/master trims are runtime values, so they are not here.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct FaderFile {
    /// `pre` | `post` — which side of the channel fader the cue send taps.
    position: Option<CueTap>,
    /// The cue/master **blend** curve (the headphones' crossfader), grouped here with
    /// `cue_blend`.
    crossfader_curve: Option<CrossfaderCurve>,
    /// `-1.0` = cue only, `0.0` = both, `1.0` = master only.
    cue_blend: Option<f32>,
}

/// The `[mixer.cue]` table: shape only. Its trim is a runtime fader.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct CueFile {
    curve: Option<CrossfaderCurve>,
    cut_width: Option<f32>,
}

impl From<MixerFile> for MixerConfig {
    fn from(file: MixerFile) -> Self {
        let fx = file.fx;
        let mut cue = CueMixConfig::default();
        cue.blend = file.fader.cue_blend.unwrap_or(-1.0);
        // The new grouping puts the blend curve in `[mixer.fader]`; `[mixer.cue] curve` remains
        // the direct spelling for the same field.
        cue.curve = file
            .fader
            .crossfader_curve
            .or(file.cue.curve)
            .unwrap_or_default();
        cue.cut_width = file.cue.cut_width.unwrap_or(super::DEFAULT_CUT_WIDTH);
        Self {
            channels: Vec::new(),
            crossfader: file.crossfader,
            flow_fx: fx.flow_fx,
            knob_fx: fx.knob_fx,
            pad_fx: fx.pad_fx,
            master_fx: fx.master_fx,
            master_limiter: file.limiter.limiter,
            master_limiter_ceiling_db: file.limiter.ceiling_db,
            cue,
            outputs: file.output,
            autogain: file.autogain,
            filter: fx.filter,
            fader_location: file.fader.position.unwrap_or_default(),
            eq: file.eq,
        }
    }
}

/// The reference topology, spelled entirely by the shipped template: `[[mixer.channel]]` for the
/// audio path and `[[multideck.deck]]` for the controller mapping. The name survives because every
/// mixer test and the CLI start from it; the topology has a single source of truth:
/// [`reference_toml`].
pub fn simple_dj() -> MixerConfig {
    MixerConfig::from_toml_str(reference_toml())
        .expect("the shipped template must parse into the reference topology")
}

/// The shipped config as TOML — the starting point for a hand-written config (the first-start
/// copy writes it verbatim). Parsing this text must yield exactly [`simple_dj`]
/// for the mixer, which is the test that keeps the schema and the constructor honest with each
/// other. The other sections are the front-end's and are ignored here.
pub fn reference_toml() -> &'static str {
    // One file is the single source of truth: the built-in topology and the first-start copy are
    // the same bytes. `from_toml_str` reads `[mixer]` and merges the
    // top-level `[[multideck.deck]]` mapping into it; the other sections are the front-end's.
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../profile/config.toml"
    ))
}

/// A single-channel config with no outputs at all: the mixer computes and writes nothing, which is
/// what makes [`Mixer::process`](super::Mixer::process) unit-testable without a sound card.
pub fn silent_test_channel() -> MixerConfig {
    MixerConfig {
        channels: vec![ChannelConfig {
            flow_fader: 0.0,
            deck_fader: 0.0,
            gain: 0.0,
            cue_send: 1.0,
            side: DeckSide::Center,
            ..Default::default()
        }],
        // `Center` on the side already takes the crossfader out of the path, so the curve is
        // irrelevant here — the default keeps the test config minimal.
        crossfader: CrossfaderConfig::default(),
        flow_fx: vec![],
        knob_fx: vec![],
        pad_fx: vec![],
        master_fx: vec![],
        master_limiter: false,
        master_limiter_ceiling_db: -1.0,
        cue: CueMixConfig::default(),
        outputs: vec![],
        autogain: AutogainSettings::default(),
        filter: FilterMode::Linear,
        fader_location: CueTap::default(),
        eq: EqConfig::default(),
    }
}

/// Instantiates a list of FX names, in order.
pub fn build_slots(names: &[String]) -> Result<Vec<FxSlot>, MixerError> {
    names.iter().map(|name| build_slot(name)).collect()
}

/// One FX by name, at its defaults. Its role is the kind's name — a bank slot gets a numbered role
/// from [`build_channel_chains`] instead.
pub fn build_slot(name: &str) -> Result<FxSlot, MixerError> {
    slot_with_role(name, None)
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
fn build_channel_chains(
    mixer: &MixerConfig,
    _cfg: &ChannelConfig,
) -> Result<BuiltChains, MixerError> {
    let (low_hz, mid_hz, high_hz) = mixer.eq.frequencies();
    let flow = (0..Stem::COUNT)
        .map(|_| {
            // `eq` first, then the sweep — part of the path, not of any bank. The EQ gets its
            // crossovers from `[mixer.eq]`; its role stays `eq` so a mapped knob survives a
            // runtime `fx select eq …` swap.
            let eq = crate::fx::sample::Eq::with_frequencies(low_hz, mid_hz, high_hz);
            let mut slots = vec![
                FxSlot::with_role(Box::new(eq), "eq", "eq"),
                slot_with_role(mixer.filter.name(), Some("filter"))?,
            ];
            for (index, slot_cfg) in mixer.flow_fx.iter().enumerate() {
                let mut slot =
                    slot_with_role(&slot_cfg.kind, Some(&format!("flowfx{}", index + 1)))?;
                slot.set_flows(slot_cfg.mask());
                slots.push(slot);
            }
            Ok(FxChain::from_slots(slots))
        })
        .collect::<Result<Vec<_>, MixerError>>()?;

    let mut post = Vec::with_capacity(mixer.knob_fx.len() + mixer.pad_fx.len());
    for (index, name) in mixer.knob_fx.iter().enumerate() {
        post.push(slot_with_role(name, Some(&format!("knobfx{}", index + 1)))?);
    }
    for (index, name) in mixer.pad_fx.iter().enumerate() {
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
        // The tone prefix is part of the path; the banks are the configurable part.
        assert_eq!(cfg.filter, FilterMode::Linear);
        assert_eq!(cfg.fader_location, CueTap::Pre, "the reference cue is pre-fader");
        assert!(cfg.flow_fx.is_empty() && cfg.knob_fx.is_empty() && cfg.pad_fx.is_empty());
        for channel in &cfg.channels {
            assert!(
                channel.cue_send > -1.0,
                "channels must reach the headphones"
            );
            assert_eq!(channel.flow_fader, 0.0, "both decks start at unity");
            assert_eq!(channel.gain, 0.0, "no channel starts with a trim");
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
    fn the_frontend_deck_count_pads_the_config_and_keeps_its_mapping() {
        let mut cfg = simple_dj();
        assert_eq!(cfg.channels.len(), 2);
        cfg.set_channel_count(4);
        assert_eq!(cfg.channels.len(), 4, "--4-decks adds two decks");
        // The decks the config maps keep their controller faces; the appended ones have none.
        assert_eq!(cfg.channels[0].source.as_deref(), Some("ctl1.A"));
        assert_eq!(cfg.channels[1].source.as_deref(), Some("ctl1.B"));
        assert_eq!(cfg.channels[2].source, None);
        assert_eq!(cfg.channels[3].source, None);
        // A new deck is audible in the headphones and sits on one side of the crossfader, with its
        // partner on the other.
        assert_eq!(cfg.channels[2].cue_send, 1.0);
        assert_eq!(cfg.channels[2].flow_fader, 0.0);
        assert_ne!(
            cfg.channels[2].side, cfg.channels[3].side,
            "the appended pair crosses the crossfader"
        );
        // Shrinking drops the tail rather than leaving a channel the front-end cannot address.
        cfg.set_channel_count(2);
        assert_eq!(cfg.channels.len(), 2);
    }

    #[test]
    fn every_named_fx_resolves() {
        let cfg = simple_dj();
        for name in cfg.all_fx() {
            build_slot(name).unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }

    #[test]
    fn the_reference_toml_parses_into_simple_dj() {
        // The contract the shipped template → edit → `--config` path depends on: that text and the
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
            // Both sides now come from the same template text, so the machine facts in it need the
            // same normalisation on each side of the comparison.
            output.device = None;
            output.channels = (0, 1);
        }
        assert_eq!(parsed, expected);
        // And it must actually build: chains resolve, limiter wires.
        assert!(parsed.build_chains().is_ok());
        assert!(parsed.master_limiter);
    }

    #[test]
    fn a_single_file_config_reads_both_top_level_sections() {
        let cfg = MixerConfig::from_toml_str(
            r#"
            [mixer]
            [[mixer.output]]
            name = "main"
            role = "master"

            [[multideck.deck]]
            source = "ctl1.A"
            side = "left"

            [live]
            audio.sample_rate = 48000

            [preference]
            theme = "dark"
            "#,
        )
        .expect("the [mixer] + [multideck] config parses");
        assert_eq!(cfg.channels.len(), 1);
        assert_eq!(cfg.outputs.len(), 1);
        assert_eq!(cfg.channels[0].side, DeckSide::Left);
        assert_eq!(cfg.channels[0].source.as_deref(), Some("ctl1.A"));
        // The other sections belong to the front-end, not the mixer's schema, so they must not
        // trip `deny_unknown_fields` or change the topology.
    }

    #[test]
    fn mixer_channel_is_refused_by_name() {
        let err =
            MixerConfig::from_toml_str("[mixer]\n[[mixer.channel]]\nside = \"left\"\n").unwrap_err();
        assert!(
            err.message().contains("[[multideck.deck]]"),
            "an old topology file must be told what moved, got: {}",
            err.message()
        );
    }

    #[test]
    fn a_config_without_a_mixer_section_uses_the_reference_topology() {
        // Sections are optional. Without `[mixer]` the front-end sections must not be mistaken
        // for a bare topology file (which used to fail with "unknown field `controller`").
        let cfg = MixerConfig::from_toml_str(
            "[live]\n[preference]\n[controller]\nmap = \"mapping.toml\"\n",
        )
        .expect("a config with no [mixer] still parses");
        assert_eq!(cfg, simple_dj());
        assert!(cfg.build_chains().is_ok());
    }

    #[test]
    fn the_struct_default_agrees_with_the_parsed_default() {
        let cfg = MixerConfig::default();
        let parsed = MixerConfig::from_toml_str("[mixer]\n[[multideck.deck]]\nside = \"center\"\n").unwrap();
        assert!(cfg.master_limiter, "the safety stage is on everywhere");
        assert_eq!(cfg.master_limiter_ceiling_db, -1.0);
        assert_eq!(cfg.master_limiter, parsed.master_limiter);
        assert_eq!(cfg.master_limiter_ceiling_db, parsed.master_limiter_ceiling_db);
    }

    #[test]
    fn the_master_limiter_is_on_by_default_and_its_ceiling_is_configurable() {
        let on = MixerConfig::from_toml_str("[mixer]\n[[multideck.deck]]\nside = \"center\"\n").unwrap();
        assert!(on.master_limiter, "omitting [mixer.limiter] must not drop the safety stage");
        assert_eq!(on.master_limiter_ceiling_db, -1.0);

        let cfg = MixerConfig::from_toml_str(
            "[mixer.limiter]\nlimiter = true\nceiling_db = -6.0\n[[multideck.deck]]\nside = \"center\"\n",
        )
        .unwrap();
        assert!(cfg.master_limiter);
        assert_eq!(cfg.master_limiter_ceiling_db, -6.0);

        let off = MixerConfig::from_toml_str(
            "[mixer.limiter]\nlimiter = false\n[[multideck.deck]]\nside = \"center\"\n",
        )
        .unwrap();
        assert!(!off.master_limiter, "a config can still turn it off");
    }

    #[test]
    fn toml_fields_default_to_the_documented_values() {
        // A minimal channel: everything optional takes the defaults the comments promise.
        let cfg = MixerConfig::from_toml_str(
            r#"
            [mixer]
            [[multideck.deck]]
            side = "center"
            [[mixer.output]]
            name = "only"
            role = "master"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.filter, FilterMode::Linear, "the clean sweep by default");
        let channel = &cfg.channels[0];
        assert!(cfg.flow_fx.is_empty() && cfg.knob_fx.is_empty() && cfg.pad_fx.is_empty());
        assert_eq!(channel.flow_fader, 0.0, "unity by default");
        assert_eq!(channel.deck_fader, 0.0);
        assert_eq!(
            channel.cue_send, 0.0,
            "a default channel does not spam the cue"
        );
        assert_eq!(
            cfg.fader_location,
            CueTap::Pre,
            "the cue taps before the fader by default, so the fader never silences the monitor"
        );
        assert_eq!(channel.gain, 0.0, "the trim starts at 0 dB");
        assert_eq!(cfg.crossfader.curve, CrossfaderCurve::Classic);
        assert_eq!(cfg.crossfader.start, 0.0);
        assert_eq!(cfg.crossfader.cut_width, crate::mixer::DEFAULT_CUT_WIDTH);
        assert_eq!(
            cfg.cue.blend, -1.0,
            "a default cue carries the cue and nothing else"
        );
        assert_eq!(cfg.outputs[0].channels, (0, 1));
        assert_eq!(cfg.outputs[0].gain, 1.0);
        assert!(
            cfg.master_limiter,
            "the safety stage is on by default; a config has to turn it off"
        );
        assert_eq!(cfg.master_limiter_ceiling_db, -1.0);
        // The struct default agrees with the parsed channel above.
        let default = ChannelConfig::default();
        assert_eq!(
            default.cue_send, 0.0,
            "a default channel must not spam the cue"
        );
        assert_eq!(default.side, DeckSide::default());
    }

    #[test]
    fn toml_errors_name_their_location_and_refuse_unknown_keys() {
        // A syntax error carries the line; a typo'd key is refused rather than ignored.
        let err = MixerConfig::from_toml_str(
            "[mixer]
[[multideck.deck]]
side = \"spinward\"",
        )
        .unwrap_err();
        assert!(matches!(err, MixerError::Parse(_)));
        assert!(err.message().contains("spinward"), "got: {}", err.message());

        let err = MixerConfig::from_toml_str("[mixer]\n[[multideck.deck]]\nside = \"left\"\nbass_boost = 12.0\n")
            .unwrap_err();
        assert!(
            err.message().contains("bass_boost"),
            "unknown keys must be named, got: {}",
            err.message()
        );

        // The pre-bank spelling is gone: `stem_fx` and `chain` are refused by name rather than
        // silently ignored, so an old topology file cannot half-load.
        for gone in ["stem_fx", "chain"] {
            let text = format!("[mixer]\n[[multideck.deck]]\n{gone} = [\"eq\"]\n");
            let err = MixerConfig::from_toml_str(&text).unwrap_err();
            assert!(
                err.message().contains(gone),
                "`{gone}` should be named in the error, got: {}",
                err.message()
            );
        }

        // An output without a name has nothing to log against.
        assert!(MixerConfig::from_toml_str("[mixer]\n[[mixer.output]]\nrole = \"main\"").is_err());
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
        cfg.knob_fx.push("shimmer".into());
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
        cfg.flow_fx = vec![FlowSlotConfig::all("gain")];
        cfg.knob_fx = vec!["echo".into(), "filter".into()];
        cfg.pad_fx = vec!["reverb".into()];
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

    /// The split `[mixer.fx]` / `[mixer.eq]` / `[mixer.limiter]` / `[mixer.fader]` shape is the
    /// documented file layout.
    #[test]
    fn the_split_mixer_tables_parse() {
        let cfg = MixerConfig::from_toml_str(
            r#"
            [mixer]

            [mixer.crossfader]
            curve = "cut"

            [mixer.fx]
            master_fx = ["echo"]
            knob_fx = ["filter"]
            pad_fx = ["reverb"]
            filter = "moog"

            [mixer.eq]
            lower_hz = 180
            higher_hz = 3_200

            [mixer.limiter]
            limiter = false
            ceiling_db = -6.0

            [mixer.fader]
            position = "post"
            crossfader_curve = "constant_power"
            cue_blend = 1.0

            [[multideck.deck]]
            side = "center"
            "#,
        )
        .expect("the split tables parse");
        assert_eq!(cfg.master_fx, vec!["echo"]);
        assert_eq!(cfg.knob_fx, vec!["filter"]);
        assert_eq!(cfg.pad_fx, vec!["reverb"]);
        assert_eq!(cfg.filter, FilterMode::Moog);
        assert_eq!(cfg.eq.lower_hz, 180.0);
        assert_eq!(cfg.eq.higher_hz, 3_200.0);
        assert!(!cfg.master_limiter);
        assert_eq!(cfg.master_limiter_ceiling_db, -6.0);
        assert_eq!(cfg.fader_location, CueTap::Post);
        assert_eq!(
            cfg.crossfader.curve,
            CrossfaderCurve::Cut,
            "the deck crossfader keeps [mixer.crossfader].curve"
        );
        assert_eq!(
            cfg.cue.curve,
            CrossfaderCurve::ConstantPower,
            "[mixer.fader].crossfader_curve shapes the cue/master blend"
        );
        assert_eq!(cfg.cue.blend, 1.0);
    }

    /// The EQ stage is built with `[mixer.eq]`'s crossovers; the middle bell is their log centre.
    #[test]
    fn the_eq_stage_uses_the_configured_crossovers() {
        let cfg = MixerConfig::from_toml_str(
            r#"
            [mixer]
            [mixer.eq]
            lower_hz = 250
            higher_hz = 2_000
            [[multideck.deck]]
            side = "center"
            "#,
        )
        .expect("parses");
        let chains = cfg.build_chains().unwrap();
        let eq = &chains[0].0[0].slots()[0];
        assert_eq!(eq.role(), "eq");
        assert_eq!(eq.fx().get_param("low_hz"), Some(250.0));
        assert_eq!(eq.fx().get_param("high_hz"), Some(2_000.0));
        let mid = eq.fx().get_param("mid_hz").unwrap();
        assert!((mid - 707.106_8).abs() < 0.01, "geometric centre, got {mid}");
    }

    /// `filter` chooses the kind of the fixed stage; the slot's role stays `filter`, which is what a
    /// mapped knob and `fx select` rely on.
    #[test]
    fn the_filter_kind_configures_the_fixed_stage() {
        let cfg = MixerConfig::from_toml_str(
            r#"
            [mixer.fx]
            filter = "moog"
            [[multideck.deck]]
            "#,
        )
        .expect("parses");
        assert_eq!(cfg.filter, FilterMode::Moog);
        let chains = cfg.build_chains().unwrap();
        let slot = &chains[0].0[0].slots()[1];
        assert_eq!(slot.kind(), "moog");
        assert_eq!(slot.role(), "filter");
    }

    /// The `flowfx` bank is a template: every flow gets its own instance of every slot.
    #[test]
    fn a_flowfx_slot_exists_once_per_flow_but_is_one_control() {
        let text = r#"
            [mixer.fx]
            knob_fx = ["filter"]
            pad_fx = []

            [[mixer.fx.flow_fx]]
            kind = "filter"

            [[mixer.fx.flow_fx]]
            kind = "echo"
            flows = ["vocals", "drums"]

            [[multideck.deck]]
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
            [mixer.fx]
            [[mixer.fx.flow_fx]]
            kind = "echo"
            flows = []

            [[multideck.deck]]
        "#,
        )
        .expect("parses");
        let chains = cfg.build_chains().unwrap();
        assert_eq!(chains[0].0[0].slots()[2].flows(), 0);

        // And an unknown name in any bank is a construction error.
        let mut cfg = cfg;
        cfg.flow_fx.push(FlowSlotConfig::all("shimmer"));
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
