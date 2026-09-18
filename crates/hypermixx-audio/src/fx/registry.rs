//! [`FxKind`]: the closed set of effect classes the engine knows how to build.
//!
//! Deliberately an enum, not a trait-object factory registry. The spec is a *fixed-order chain*, so
//! the only thing a front-end needs is a name → instance mapping and a list for `help`; a dynamic
//! registry would add a plugin surface with nothing to plug into it. Adding an effect means one
//! variant, one `build` arm and one line in `ALL`.

use super::sample::{Eq, Filter, Gain, Limiter};
use super::{Fx, FxError};

/// Every built-in effect class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FxKind {
    /// 3-band DJ EQ (bass / mid / treble).
    Eq,
    /// Resonant low-pass / high-pass / band-pass sweep.
    Filter,
    /// Straight gain, ±∞ dB law.
    Gain,
    /// Safety limiter with a soft knee and a releasing threshold.
    Limiter,
}

impl FxKind {
    /// Every kind, in the order `help` should list them.
    pub const ALL: [FxKind; 4] = [FxKind::Eq, FxKind::Filter, FxKind::Gain, FxKind::Limiter];

    /// The canonical name, and the only spelling [`FxKind::parse`] accepts.
    pub fn name(&self) -> &'static str {
        match self {
            FxKind::Eq => "eq",
            FxKind::Filter => "filter",
            FxKind::Gain => "gain",
            FxKind::Limiter => "limiter",
        }
    }

    /// Names this kind answers to, including the aliases a human types.
    pub fn aliases(&self) -> &'static [&'static str] {
        match self {
            FxKind::Eq => &["eq", "equalizer", "tone"],
            FxKind::Filter => &["filter", "lp", "hp", "sweep"],
            FxKind::Gain => &["gain", "volume", "amp"],
            FxKind::Limiter => &["limiter", "lim", "clip"],
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
            FxKind::Gain => Box::new(Gain::new(1.0)),
            FxKind::Limiter => Box::new(Limiter::new()),
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
            FxKind::Gain => Gain::PARAMS,
            FxKind::Limiter => Limiter::PARAMS,
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
        let err = FxKind::parse("reverb").unwrap_err();
        assert_eq!(err, FxError::UnknownKind("reverb".into()));
        assert!(err.message().contains("reverb"));
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
