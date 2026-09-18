//! [`Eq`]: the three-band tone control every DJ mix depends on.
//!
//! Topology, low to high: a **low shelf** at 320 Hz, a **bell** at 1.2 kHz, a **high shelf** at
//! 3.6 kHz. Shelves at the ends because a DJ wants "all the bass out", not "a dip near 200 Hz"; a
//! bell in the middle because a mid band with no centre frequency is just a second shelf.
//!
//! Fader law: `-1.0 ..= 1.0`, mapped *linearly in dB* onto ±[`MAX_BAND_DB`] — the taper a hardware
//! ISO is labelled with. It stops short of digital silence on purpose: a shelf cutting 120 dB has a
//! transition so wide that its skirt reaches up into the neighbouring band, so a kill the width of a
//! continent colours the treble it is not touching. ±26 dB is a full kill in practice, and a channel
//! fader is where true silence belongs.

use super::super::{Fx, FxContext, FxError, Param};
use crate::fx::dsp::{BiquadChain, Section};
use crate::mixer::Bus;

/// Full band gain in either direction, dB. 26 is the classic ISO taper: enough to make a band
/// disappear next to a neighbour, shallow enough that its transition stays inside its own region.
pub const MAX_BAND_DB: f32 = 26.0;

/// Factory crossover frequencies, matching the spread of a typical DJ channel EQ.
pub const DEFAULT_LOW_HZ: f32 = 320.0;
pub const DEFAULT_MID_HZ: f32 = 1_200.0;
pub const DEFAULT_HIGH_HZ: f32 = 3_600.0;

/// A 3-band tone control.
pub struct Eq {
    low: Param,
    mid: Param,
    high: Param,
    low_hz: Param,
    mid_hz: Param,
    high_hz: Param,
    chain: BiquadChain,
    /// Crossovers and band gains as of the last block. Coefficients are recomputed only when one of
    /// these actually changes — see [`Eq::redesign_count`].
    cached_hertz: [f32; 3],
    cached_db: [f64; 3],
    redesigns: u32,
}

impl Eq {
    /// A flat (unity) EQ at the factory frequencies.
    pub fn new() -> Self {
        Self::with_frequencies(DEFAULT_LOW_HZ, DEFAULT_MID_HZ, DEFAULT_HIGH_HZ)
    }

    pub fn with_frequencies(low_hz: f32, mid_hz: f32, high_hz: f32) -> Self {
        let initial = [low_hz, mid_hz, high_hz];
        Self {
            low: Param::new(0.0, EQ_TAU),
            mid: Param::new(0.0, EQ_TAU),
            high: Param::new(0.0, EQ_TAU),
            low_hz: Param::instant(low_hz),
            mid_hz: Param::instant(mid_hz),
            high_hz: Param::instant(high_hz),
            chain: BiquadChain::from_design(
                &eq_sections(low_hz, mid_hz, high_hz, 0.0, 0.0, 0.0),
                crate::SAMPLE_RATE,
            ),
            cached_hertz: initial,
            cached_db: [0.0; 3],
            redesigns: 0,
        }
    }

    /// Parameter names, in fader order.
    pub const PARAMS: &'static [&'static str] =
        &["low", "mid", "high", "low_hz", "mid_hz", "high_hz"];

    /// Band gains currently applied, in dB. Meter source and test hook.
    pub fn band_db(&self) -> [f64; 3] {
        [
            band_db_of(self.low.get()),
            band_db_of(self.mid.get()),
            band_db_of(self.high.get()),
        ]
    }

    /// How many times the coefficient set has been recomputed. A static EQ must stay at one.
    pub fn redesign_count(&self) -> u32 {
        self.redesigns
    }

    fn set_band(param: &Param, value: f32) -> Result<(), FxError> {
        if !value.is_finite() {
            return Err(FxError::InvalidValue(value));
        }
        param.set(value.clamp(-1.0, 1.0));
        Ok(())
    }

    fn set_hz(param: &Param, value: f32) -> Result<(), FxError> {
        if !value.is_finite() || value <= 0.0 {
            return Err(FxError::InvalidValue(value));
        }
        // Below Nyquist, above the sub-audible.
        param.set(value.clamp(20.0, crate::SAMPLE_RATE as f32 * 0.45));
        Ok(())
    }
}

impl Default for Eq {
    fn default() -> Self {
        Self::new()
    }
}

/// Enough to hide a stepped fader move, short enough that an EQ sweep during a blend is still the
/// sweep the hand performed rather than a lagging copy of it.
const EQ_TAU: f32 = 0.008;

/// Maximally-flat shelf slope: the classic 6 dB/oct-per-stage tone control, and the only value
/// range the design guarantees stays real at any gain.
const SHELF_SLOPE: f64 = 1.0;

/// A fader position `-1.0 ..= 1.0` to band gain in dB, linearly (an ISO taper).
#[inline]
fn band_db_of(position: f32) -> f64 {
    f64::from(position.clamp(-1.0, 1.0) * MAX_BAND_DB)
}

fn eq_sections(
    low_hz: f32,
    mid_hz: f32,
    high_hz: f32,
    low_db: f64,
    mid_db: f64,
    high_db: f64,
) -> Vec<Section> {
    vec![
        Section::LowShelf { freq: f64::from(low_hz), gain_db: low_db, slope: SHELF_SLOPE },
        Section::Bell { freq: f64::from(mid_hz), gain_db: mid_db, q: 1.0 },
        Section::HighShelf { freq: f64::from(high_hz), gain_db: high_db, slope: SHELF_SLOPE },
    ]
}

impl Fx for Eq {
    fn process(&mut self, bus: &mut Bus, ctx: &FxContext) {
        let (frames, sr) = (ctx.block_frames, ctx.sample_rate);
        let (low_pos, mid_pos, high_pos) = (
            self.low.next_block(frames, sr),
            self.mid.next_block(frames, sr),
            self.high.next_block(frames, sr),
        );
        let hertz = [
            self.low_hz.next_block(frames, sr),
            self.mid_hz.next_block(frames, sr),
            self.high_hz.next_block(frames, sr),
        ];
        let db = [
            band_db_of(low_pos),
            band_db_of(mid_pos),
            band_db_of(high_pos),
        ];

        // Coefficients are recomputed only when a control moved, and *in place*: the filter memory
        // survives a sweep, which is what lets a band settle while its knob travels.
        if hertz != self.cached_hertz || db != self.cached_db {
            let design = eq_sections(hertz[0], hertz[1], hertz[2], db[0], db[1], db[2]);
            if !self.chain.apply_design(&design, sr) {
                self.chain = BiquadChain::from_design(&design, sr);
            }
            self.cached_hertz = hertz;
            self.cached_db = db;
            self.redesigns += 1;
        }
        // One diverged section would otherwise mute this deck for the rest of the session.
        self.chain.recover_if_blowing(|| {
            eq_sections(hertz[0], hertz[1], hertz[2], db[0], db[1], db[2])
                .iter()
                .map(|section| section.build(sr))
                .collect()
        });

        if db == [0.0, 0.0, 0.0] {
            // Flat: three unity sections, so the innermost loop can be skipped entirely.
            return;
        }
        for (channel, plane) in [(0usize, &mut bus.l), (1, &mut bus.r)] {
            for sample in plane.iter_mut() {
                *sample = self.chain.process_sample(channel, *sample);
            }
        }
    }

    fn reset(&mut self) {
        self.low.snap(self.low.target());
        self.mid.snap(self.mid.target());
        self.high.snap(self.high.target());
        self.chain.clear();
        // Force a rebuild from the now-settled values on the next block.
        self.cached_db = [f64::NAN; 3];
    }

    fn set_param(&self, name: &str, value: f32) -> Result<(), FxError> {
        match name {
            "low" => Self::set_band(&self.low, value),
            "mid" => Self::set_band(&self.mid, value),
            "high" => Self::set_band(&self.high, value),
            "low_hz" => Self::set_hz(&self.low_hz, value),
            "mid_hz" => Self::set_hz(&self.mid_hz, value),
            "high_hz" => Self::set_hz(&self.high_hz, value),
            _ => Err(FxError::UnknownParam),
        }
    }

    fn get_param(&self, name: &str) -> Option<f32> {
        match name {
            "low" => Some(self.low.target()),
            "mid" => Some(self.mid.target()),
            "high" => Some(self.high.target()),
            "low_hz" => Some(self.low_hz.target()),
            "mid_hz" => Some(self.mid_hz.target()),
            "high_hz" => Some(self.high_hz.target()),
            _ => None,
        }
    }

    fn param_names(&self) -> &'static [&'static str] {
        Self::PARAMS
    }
}

impl std::fmt::Debug for Eq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let db = self.band_db();
        f.debug_struct("Eq")
            .field("low_db", &(db[0] as f32))
            .field("mid_db", &(db[1] as f32))
            .field("high_db", &(db[2] as f32))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SAMPLE_RATE;

    const BLOCK: usize = 256;

    fn sine(freq: f64, n: usize) -> f32 {
        (2.0 * std::f64::consts::PI * freq * n as f64 / f64::from(SAMPLE_RATE)).sin() as f32
    }

    /// Feeds `freq` through the EQ in engine-shaped blocks and returns the tail's peak in dB.
    ///
    /// Blocks of `BLOCK` because that is how the mixer calls it; a tail window because the first
    /// half of the run is the filter settling, which is exactly what the flat-vs-shaped tests must
    /// not measure.
    fn measure(eq: &mut Eq, freq: f64, blocks: usize) -> f64 {
        let ctx = FxContext::gridless(SAMPLE_RATE, BLOCK);
        let mut tail = Vec::with_capacity(BLOCK);
        for b in 0..blocks {
            let mut bus = Bus::new(BLOCK);
            for (i, slot) in bus.l.iter_mut().enumerate() {
                let s = sine(freq, b * BLOCK + i);
                *slot = s;
                bus.r[i] = s;
            }
            eq.process(&mut bus, &ctx);
            if b >= blocks / 2 {
                tail.clear();
                tail.extend_from_slice(&bus.l);
                assert!(
                    tail.iter().all(|s| s.is_finite()),
                    "EQ emitted a non-finite sample at {freq}Hz"
                );
            }
        }
        let peak = tail.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        20.0 * f64::from(peak.max(1e-12)).log10()
    }

    fn settled(eq: &mut Eq, freq: f64) -> f64 {
        measure(eq, freq, 120)
    }

    #[test]
    fn band_law_is_linear_in_db_and_centred() {
        // Half the travel is half the range: the taper a knob is labelled with. Checked on the law
        // itself, since `band_db` reports the *smoothed* value and nothing has stepped it yet.
        assert_eq!(band_db_of(0.0), 0.0, "centred is unity");
        assert!(
            (band_db_of(0.5) - f64::from(MAX_BAND_DB) * 0.5).abs() < 1e-6,
            "half-travel gave {:.1} dB",
            band_db_of(0.5)
        );
        assert!((band_db_of(1.0) - f64::from(MAX_BAND_DB)).abs() < 1e-6);
        assert!((band_db_of(-1.0) + f64::from(MAX_BAND_DB)).abs() < 1e-6);
        // Monotonic and clamped at both ends of the travel.
        let mut prev = f64::NEG_INFINITY;
        for i in -50..=50 {
            let v = band_db_of(i as f32 / 40.0); // deliberately runs past +-1
            assert!(v >= prev, "the taper folded back at {i}");
            assert!(v.abs() <= f64::from(MAX_BAND_DB) + 1e-6, "out of range: {v}");
            prev = v;
        }
        let eq = Eq::new();
        assert_eq!(eq.get_param("low"), Some(0.0), "a fresh EQ is flat");
    }

    #[test]
    fn flat_is_transparent_across_the_band() {
        let mut eq = Eq::new();
        for probe in [100.0, 1_000.0, 8_000.0] {
            let db = settled(&mut eq, probe);
            assert!(db.abs() < 0.5, "flat EQ measured {db:.2} dB at {probe}Hz");
        }
    }

    #[test]
    fn full_cc_kills_only_its_own_band() {
        let mut eq = Eq::new();
        eq.set_param("low", -1.0).unwrap();
        let bass = settled(&mut eq, 80.0);
        assert!(
            bass < -(MAX_BAND_DB as f64) * 0.8,
            "a killed bass band only reached {bass:.1} dB"
        );
        // The whole point of a *shelf* rather than a steep cut: the treble is untouched.
        let treble = settled(&mut eq, 8_000.0);
        assert!(treble > -2.0, "killing bass coloured treble by {treble:.1} dB");

        // And the mirror for the treble band.
        let mut eq = Eq::new();
        eq.set_param("high", -1.0).unwrap();
        assert!(settled(&mut eq, 12_000.0) < -(MAX_BAND_DB as f64) * 0.8, "treble kill");
        assert!(settled(&mut eq, 80.0) > -2.0, "killing treble coloured bass");
    }

    #[test]
    fn each_band_spans_cut_to_boost_in_its_own_region() {
        for (band, probe) in [("low", 80.0), ("mid", 1_200.0), ("high", 8_000.0)] {
            let mut boosted = Eq::new();
            boosted.set_param(band, 1.0).unwrap();
            let up = settled(&mut boosted, probe);
            let mut cut = Eq::new();
            cut.set_param(band, -1.0).unwrap();
            let down = settled(&mut cut, probe);
            assert!(
                up - down > 20.0,
                "{band}: boost {up:.1} dB vs cut {down:.1} dB at {probe}Hz is not a band"
            );
        }
    }

    #[test]
    fn mid_is_a_bell_not_a_shelf() {
        // A mid boost must fall off again above the treble crossover, which a shelf would not.
        let mut eq = Eq::new();
        eq.set_param("mid", 1.0).unwrap();
        let at_peak = settled(&mut eq, 1_200.0);
        let far_above = settled(&mut eq, 12_000.0);
        assert!(at_peak > 6.0, "mid boost only reached {at_peak:.1} dB");
        assert!(
            far_above < at_peak - 6.0,
            "mid did not roll off again: {far_above:.1} dB at 12k vs {at_peak:.1} at 1.2k"
        );
    }

    #[test]
    fn crossover_frequencies_are_tunable() {
        let mut eq = Eq::with_frequencies(80.0, 300.0, 900.0);
        eq.set_param("high", 1.0).unwrap();
        let boosted = settled(&mut eq, 4_000.0);
        assert!(boosted > 6.0, "retuned shelf did not follow: {boosted:.1} dB");

        // And the same boost at factory frequencies must NOT touch 4 kHz as strongly.
        let mut stock = Eq::new();
        stock.set_param("high", 1.0).unwrap();
        let stock_gain = settled(&mut stock, 4_000.0);
        assert!(
            boosted > stock_gain + 3.0,
            "retuning 3.6k -> 900Hz changed nothing ({stock_gain:.1} vs {boosted:.1})"
        );
    }

    #[test]
    fn coefficients_are_recomputed_only_when_a_control_moves() {
        // The invariant the design depends on: per-block rebuilds would cost needless `sin`/`cos`
        // and, worse, invite a state-clearing regression.
        let mut eq = Eq::new();
        eq.set_param("low", -0.5).unwrap();
        let ctx = FxContext::gridless(SAMPLE_RATE, BLOCK);
        let mut bus = Bus::new(BLOCK);
        for _ in 0..60 {
            eq.process(&mut bus, &ctx);
        }
        let during_ramp = eq.redesign_count();
        assert!(during_ramp >= 2, "the ramp should have redrawn a few times");
        for _ in 0..60 {
            eq.process(&mut bus, &ctx);
        }
        assert_eq!(
            eq.redesign_count(),
            during_ramp,
            "a settled EQ must not recompute coefficients"
        );
    }

    #[test]
    fn param_surface_is_strict() {
        let eq = Eq::new();
        assert_eq!(eq.set_param("bass", 0.5), Err(FxError::UnknownParam));
        assert!(matches!(
            eq.set_param("low", f32::NAN),
            Err(FxError::InvalidValue(_))
        ));
        assert!(matches!(
            eq.set_param("low_hz", -5.0),
            Err(FxError::InvalidValue(_))
        ));
        eq.set_param("low", -99.0).unwrap();
        assert_eq!(eq.get_param("low"), Some(-1.0), "clamped to the fader's travel");
        eq.set_param("low_hz", 1e9).unwrap();
        assert!(eq.get_param("low_hz").unwrap() < SAMPLE_RATE as f32);
        assert_eq!(eq.param_names(), Eq::PARAMS);
        assert_eq!(eq.param_names().len(), 6);
    }

    #[test]
    fn reset_settles_the_bands_and_clears_history() {
        let mut eq = Eq::new();
        eq.set_param("low", -1.0).unwrap();
        let ctx = FxContext::gridless(SAMPLE_RATE, BLOCK);
        let mut bus = Bus::new(BLOCK);
        eq.process(&mut bus, &ctx);
        eq.reset();
        assert!(
            (eq.low.get() + 1.0).abs() < 1e-6,
            "reset must land a band on its target, got {}",
            eq.low.get()
        );
        assert_eq!(eq.chain.blowup_count(), 0);
    }
}
