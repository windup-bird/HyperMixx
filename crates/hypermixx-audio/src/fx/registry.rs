//! [`FxKind`]: the closed set of effect classes the engine knows how to build.
//!
//! Deliberately an enum, not a trait-object factory registry. The spec is a *fixed-order chain*, so
//! the only thing a front-end needs is a name → instance mapping and a list for `help`; a dynamic
//! registry would add a plugin surface with nothing to plug into it. Adding an effect means one
//! variant, one `build` arm and one line in `ALL`.

use super::sample::{Cut, Echo, Eq, Filter, Flanger, Gain, Limiter, Phaser, Reverb};
use super::{Fx, FxError};

/// Every built-in effect class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FxKind {
    /// 3-band DJ EQ (bass / mid / treble).
    Eq,
    /// Resonant low-pass / high-pass sweep, two poles (the clean "linear" sweep).
    Filter,
    /// The same sweep with four poles and a ladder character.
    Moog,
    /// Straight gain, ±∞ dB law.
    Gain,
    /// Safety limiter with a soft knee and a releasing threshold.
    Limiter,
    /// Tempo-synced feedback delay with a damped feedback path.
    Echo,
    /// Freeverb-style stereo room.
    Reverb,
    /// Modulated short delay (the jet sweep).
    Flanger,
    /// Swept all-pass cascade (the moving notches).
    Phaser,
    /// Beat-locked gate: the DJM's Trans, in one insert.
    Cut,
}

impl FxKind {
    /// Every kind, in the order `help` should list them.
    pub const ALL: [FxKind; 10] = [
        FxKind::Eq,
        FxKind::Filter,
        FxKind::Moog,
        FxKind::Gain,
        FxKind::Limiter,
        FxKind::Echo,
        FxKind::Reverb,
        FxKind::Flanger,
        FxKind::Phaser,
        FxKind::Cut,
    ];

    /// The canonical name, and the only spelling [`FxKind::parse`] accepts.
    pub fn name(&self) -> &'static str {
        match self {
            FxKind::Eq => "eq",
            FxKind::Filter => "filter",
            FxKind::Moog => "moog",
            FxKind::Gain => "gain",
            FxKind::Limiter => "limiter",
            FxKind::Echo => "echo",
            FxKind::Reverb => "reverb",
            FxKind::Flanger => "flanger",
            FxKind::Phaser => "phaser",
            FxKind::Cut => "cut",
        }
    }

    /// Names this kind answers to, including the aliases a human types.
    pub fn aliases(&self) -> &'static [&'static str] {
        match self {
            FxKind::Eq => &["eq", "equalizer", "tone"],
            FxKind::Filter => &["filter", "lp", "hp", "sweep", "linear"],
            FxKind::Moog => &["moog", "ladder"],
            FxKind::Gain => &["gain", "volume", "amp"],
            FxKind::Limiter => &["limiter", "lim", "clip"],
            // "delay" is the plain-language name for what this kind actually is; a future
            // no-feedback delay would be a separate kind, not a second meaning for this alias.
            FxKind::Echo => &["echo", "delay", "dub"],
            FxKind::Reverb => &["reverb", "verb"],
            FxKind::Flanger => &["flanger", "flange"],
            FxKind::Phaser => &["phaser", "phase"],
            FxKind::Cut => &["cut", "gate", "trans"],
        }
    }

    /// Accepts a canonical name or any alias, case-insensitively.
    pub fn parse(name: &str) -> Result<Self, FxError> {
        let needle = name.trim().to_ascii_lowercase();
        FxKind::ALL
            .into_iter()
            .find(|kind| kind.aliases().iter().any(|a| *a == needle))
            .ok_or(FxError::UnknownKind(name.to_owned()))
    }

    /// A fresh instance at its default settings.
    pub fn build(&self) -> Box<dyn Fx> {
        match self {
            FxKind::Eq => Box::new(Eq::new()),
            FxKind::Filter => Box::new(Filter::new()),
            FxKind::Moog => Box::new(Filter::moog()),
            FxKind::Gain => Box::new(Gain::new(1.0)),
            FxKind::Limiter => Box::new(Limiter::new()),
            FxKind::Echo => Box::new(Echo::new()),
            FxKind::Reverb => Box::new(Reverb::new()),
            FxKind::Flanger => Box::new(Flanger::new()),
            FxKind::Phaser => Box::new(Phaser::new()),
            FxKind::Cut => Box::new(Cut::new()),
        }
    }

    /// Parameter names this kind exposes, without constructing it — used to validate a `SetFxParam`
    /// before it reaches a slot, and to print `fx help eq`.
    pub fn param_names(&self) -> &'static [&'static str] {
        // Defaults are cheap (no allocation beyond a Vec of Params), so building one is the honest
        // way to keep this list and the instance's own list from ever disagreeing.
        match self {
            FxKind::Eq => Eq::PARAMS,
            FxKind::Filter => Filter::PARAMS,
            FxKind::Moog => Filter::PARAMS,
            FxKind::Gain => Gain::PARAMS,
            FxKind::Limiter => Limiter::PARAMS,
            FxKind::Echo => Echo::PARAMS,
            FxKind::Reverb => Reverb::PARAMS,
            FxKind::Flanger => Flanger::PARAMS,
            FxKind::Phaser => Phaser::PARAMS,
            FxKind::Cut => Cut::PARAMS,
        }
    }

    pub fn is_known(name: &str) -> bool {
        Self::parse(name).is_ok()
    }
}

impl std::fmt::Display for FxKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_builds_and_names_itself() {
        for kind in FxKind::ALL {
            let fx = kind.build();
            assert!(!kind.name().is_empty());
            assert_eq!(
                fx.param_names(),
                kind.param_names(),
                "{kind}: registry list disagrees with the instance"
            );
            // Every declared parameter must be readable and writable.
            for name in kind.param_names() {
                assert!(
                    fx.get_param(name).is_some(),
                    "{kind}: {name} declared but not readable"
                );
                let current = fx.get_param(name).unwrap();
                assert!(
                    fx.set_param(name, current).is_ok(),
                    "{kind}: refused its own current {name}"
                );
            }
        }
    }

    #[test]
    fn parse_accepts_aliases_and_case_but_nothing_invented() {
        assert_eq!(FxKind::parse("EQ"), Ok(FxKind::Eq));
        assert_eq!(FxKind::parse("  limiter "), Ok(FxKind::Limiter));
        assert_eq!(FxKind::parse("lp"), Ok(FxKind::Filter));
        assert_eq!(FxKind::parse("tone"), Ok(FxKind::Eq));
        // The time-based effects answer to both their name and the spelling a controller map uses.
        assert_eq!(FxKind::parse("delay"), Ok(FxKind::Echo));
        assert_eq!(FxKind::parse("verb"), Ok(FxKind::Reverb));
        assert_eq!(FxKind::parse("flange"), Ok(FxKind::Flanger));
        assert_eq!(FxKind::parse("phase"), Ok(FxKind::Phaser));
        assert_eq!(FxKind::parse("gate"), Ok(FxKind::Cut));
        assert_eq!(FxKind::parse("trans"), Ok(FxKind::Cut));
        // A name that is still not an effect is refused, and the message names it.
        let err = FxKind::parse("shimmer").unwrap_err();
        assert_eq!(err, FxError::UnknownKind("shimmer".into()));
        assert!(err.message().contains("shimmer"));
        assert!(!FxKind::is_known("scratcher"));
    }

    #[test]
    fn names_are_unique_across_the_registry() {
        let mut names: Vec<_> = FxKind::ALL.iter().map(|k| k.name()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "two kinds share a name");
    }
}
