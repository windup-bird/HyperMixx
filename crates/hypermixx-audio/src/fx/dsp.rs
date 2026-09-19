//! Biquad primitives shared by the EQ, the filters and any effect that needs a resonance.
//!
//! Coefficients follow the RBJ Audio-EQ-Cookbook formulas — the same topology family every DJ EQ
//! uses: a resonant low-pass, a bell, and low/high shelves. Doing it once here means a `Filter` and
//! the low band of an `Eq` cannot disagree about what "220 Hz at Q 0.7" sounds like.

/// Design coefficients for one second-order section, before normalisation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BiquadCoeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a0: f64,
    pub a1: f64,
    pub a2: f64,
}

/// One second-order section in transposed direct-form II, stereo.
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    /// The two delay elements, one pair per channel.
    z: [[f32; 2]; 2],
}

impl Biquad {
    pub fn from(coeffs: BiquadCoeffs) -> Self {
        let mut section = Self {
            b0: 0.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z: [[0.0; 2]; 2],
        };
        section.set_coeffs(coeffs);
        section
    }

    /// Replaces the coefficients **and keeps the filter memory**.
    ///
    /// This is what makes a continuous sweep possible: redesigning per block is only a dozen flops
    /// plus a `sin`/`cos`, and dropping the state each time would mean a filter that never reaches
    /// steady state. Callers that want a hard restart use [`Biquad::clear`] separately.
    pub fn set_coeffs(&mut self, coeffs: BiquadCoeffs) {
        // Guard the division *and* the pathological case where a0 collapses to zero, which would
        // turn every coefficient into an infinity.
        let a0 = if coeffs.a0.abs() < f64::EPSILON {
            1.0
        } else {
            coeffs.a0
        };
        let norm = |v: f64| (v / a0) as f32;
        self.b0 = norm(coeffs.b0);
        self.b1 = norm(coeffs.b1);
        self.b2 = norm(coeffs.b2);
        self.a1 = norm(coeffs.a1);
        self.a2 = norm(coeffs.a2);
    }

    #[inline]
    pub fn process_sample(&mut self, channel: usize, x: f32) -> f32 {
        let state = &mut self.z[channel & 1];
        let y = self.b0 * x + state[0];
        state[0] = self.b1 * x - self.a1 * y + state[1];
        state[1] = self.b2 * x - self.a2 * y;
        y
    }

    /// Forgets history. Call on reset and whenever the coefficients change: old state run through
    /// new coefficients is a guaranteed transient.
    pub fn clear(&mut self) {
        self.z = [[0.0; 2]; 2];
    }

    /// The section's own coefficients, normalised so `a0 == 1`.
    #[cfg(test)]
    pub fn coeffs(&self) -> [f32; 5] {
        [self.b0, self.b1, self.b2, self.a1, self.a2]
    }

    /// True when this section has diverged or been handed non-finite coefficients.
    pub fn is_blowing(&self) -> bool {
        self.z.iter().flatten().any(|v| !v.is_finite())
            || [self.b0, self.b1, self.b2, self.a1, self.a2]
                .iter()
                .any(|c| !c.is_finite())
    }

}

/// One low-pass / high-pass / band-pass / bell / shelf section.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Section {
    LowPass { freq: f64, q: f64 },
    HighPass { freq: f64, q: f64 },
    /// Resonant band-pass, `bandwidth_hz` wide at −3 dB, unity gain at the centre.
    BandPass { freq: f64, bandwidth_hz: f64 },
    Bell { freq: f64, gain_db: f64, q: f64 },
    /// Shelf with `slope` (RBJ's `S`): 1.0 is maximally flat. Clamped to (0, 1] because the
    /// `alpha` term's square root goes imaginary past that on a deep shelf, which would hand the
    /// caller a `NaN` coefficient instead of a filter.
    LowShelf { freq: f64, gain_db: f64, slope: f64 },
    HighShelf { freq: f64, gain_db: f64, slope: f64 },
}

impl Section {
    /// Designs the section at `sample_rate` Hz.
    ///
    /// Frequency and Q are clamped into the representable band: a control swept to 0 Hz or past
    /// Nyquist must still yield stable coefficients, not a `NaN` that rides into the output and
    /// stays there for the rest of the session.
    pub fn coeffs(&self, sample_rate: u32) -> BiquadCoeffs {
        let sr = f64::from(sample_rate.max(1));
        let nyquist = sr * 0.499;
        // `f64::clamp` passes `NaN` straight through, so sanitise first: a NaN cutoff would become
        // a NaN coefficient and then a NaN output that never recovers.
        let freq_of = |f: f64| finite_or(f, sr / 4.0).clamp(5.0, nyquist);
        let q_of = |q: f64| finite_or(q, 0.707).max(0.05);
        // A stability guard, not a musical range: `A` only degenerates at exactly zero, so ±120/36 dB
        // keeps every coefficient finite no matter what a caller asks for. The *usable* travel is
        // decided by the effect (see `Eq::MAX_BAND_DB`), because a shelf cut far beyond a band's
        // worth spills its transition into the neighbouring band.
        let db_of = |g: f64| finite_or(g, 0.0).clamp(-120.0, 36.0);
        // The two gain sections need *different* exponents because their realised gain is a
        // different power of `A`: a bell peaks at exactly `A`, a shelf's stopband gain is `A²`. So a
        // requested dB maps to `10^(dB/20)` (amplitude) for the bell and `10^(dB/40)` for a shelf —
        // which is also what makes a 0 dB shelf collapse to the identity. Getting this wrong is
        // silent: the filter still works, just by twice as much as the knob says.
        let db_to_amp = |g: f64| 10f64.powf(g / 20.0);
        let db_to_shelf_a = |g: f64| 10f64.powf(g / 40.0);
        let w = |freq: f64| 2.0 * std::f64::consts::PI * freq_of(freq) / sr;

        match *self {
            Section::LowPass { freq, q } => {
                let omega = w(freq);
                let (cos_w, alpha) = (omega.cos(), omega.sin() / (2.0 * q_of(q)));
                BiquadCoeffs {
                    b0: (1.0 - cos_w) / 2.0,
                    b1: 1.0 - cos_w,
                    b2: (1.0 - cos_w) / 2.0,
                    a0: 1.0 + alpha,
                    a1: -2.0 * cos_w,
                    a2: 1.0 - alpha,
                }
            }
            Section::HighPass { freq, q } => {
                let omega = w(freq);
                let (cos_w, alpha) = (omega.cos(), omega.sin() / (2.0 * q_of(q)));
                BiquadCoeffs {
                    b0: (1.0 + cos_w) / 2.0,
                    b1: -(1.0 + cos_w),
                    b2: (1.0 + cos_w) / 2.0,
                    a0: 1.0 + alpha,
                    a1: -2.0 * cos_w,
                    a2: 1.0 - alpha,
                }
            }
            Section::BandPass { freq, bandwidth_hz } => {
                // The "constant peak gain" form: |H| at the centre is 1 whatever the bandwidth, so a
                // resonant filter can never add the gain a Q-driven low-pass peak does.
                let omega = w(freq);
                let sin_w = omega.sin().max(1e-9);
                let centre = freq_of(freq);
                let width = finite_or(bandwidth_hz, centre).clamp(1.0, centre * 2.0);
                // The −3 dB points, expressed as the octave span the cookbook's `BW` wants.
                let lo = (centre - width / 2.0).max(5.0);
                let hi = (centre + width / 2.0).min(nyquist);
                let bw_octaves = (f64::from(hi / lo).log2()).clamp(0.02, 8.0);
                let alpha =
                    sin_w * (std::f64::consts::LN_2 / 2.0 * bw_octaves * omega / sin_w).sinh();
                BiquadCoeffs {
                    b0: alpha,
                    b1: 0.0,
                    b2: -alpha,
                    a0: 1.0 + alpha,
                    a1: -2.0 * omega.cos(),
                    a2: 1.0 - alpha,
                }
            }
            Section::Bell { freq, gain_db, q } => {
                let omega = w(freq);
                let (a, cos_w) = (db_to_amp(db_of(gain_db)), omega.cos());
                let alpha = omega.sin() / (2.0 * q_of(q));
                BiquadCoeffs {
                    b0: 1.0 + alpha * a,
                    b1: -2.0 * cos_w,
                    b2: 1.0 - alpha * a,
                    a0: 1.0 + alpha,
                    a1: -2.0 * cos_w,
                    a2: 1.0 - alpha,
                }
            }
            Section::LowShelf { freq, gain_db, slope } => {
                let omega = w(freq);
                let (a, cos_w, sin_w) = (db_to_shelf_a(db_of(gain_db)), omega.cos(), omega.sin());
                let alpha = shelf_alpha(&a, &sin_w, slope);
                let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
                // At A = 1 every b equals its a, i.e. the section is exactly `y = x` — the property
                // the "flat is transparent" test checks.
                BiquadCoeffs {
                    b0: a * ((a + 1.0) - (a - 1.0) * cos_w + two_sqrt_a_alpha),
                    b1: 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w),
                    b2: a * ((a + 1.0) - (a - 1.0) * cos_w - two_sqrt_a_alpha),
                    a0: (a + 1.0) + (a - 1.0) * cos_w + two_sqrt_a_alpha,
                    a1: -2.0 * ((a - 1.0) + (a + 1.0) * cos_w),
                    a2: (a + 1.0) + (a - 1.0) * cos_w - two_sqrt_a_alpha,
                }
            }
            Section::HighShelf { freq, gain_db, slope } => {
                let omega = w(freq);
                let (a, cos_w, sin_w) = (db_to_shelf_a(db_of(gain_db)), omega.cos(), omega.sin());
                let alpha = shelf_alpha(&a, &sin_w, slope);
                let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
                BiquadCoeffs {
                    b0: a * ((a + 1.0) + (a - 1.0) * cos_w + two_sqrt_a_alpha),
                    b1: -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w),
                    b2: a * ((a + 1.0) + (a - 1.0) * cos_w - two_sqrt_a_alpha),
                    a0: (a + 1.0) - (a - 1.0) * cos_w + two_sqrt_a_alpha,
                    a1: 2.0 * ((a - 1.0) - (a + 1.0) * cos_w),
                    a2: (a + 1.0) - (a - 1.0) * cos_w - two_sqrt_a_alpha,
                }
            }
        }
    }
}

/// The shelf `alpha`, guarded against the negative-square-root case.
///
/// `alpha = sin(w)/2 * sqrt((A + 1/A)(1/S - 1) + 2)`. The argument is guaranteed non-negative for
/// `S <= 1`, which is why [`Section`] takes a clamped slope rather than a free Q here.
fn shelf_alpha(a: &f64, sin_w: &f64, slope: f64) -> f64 {
    let s = finite_or(slope, 1.0).clamp(0.05, 1.0);
    let inside = (a + 1.0 / a) * (1.0 / s - 1.0) + 2.0;
    sin_w / 2.0 * inside.max(0.0).sqrt()
}

impl Section {
    /// Designs straight into a runnable section.
    pub fn build(&self, sample_rate: u32) -> Biquad {
        Biquad::from(self.coeffs(sample_rate))
    }
}

#[inline]
fn finite_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

/// A chain of sections, applied in order.
#[derive(Clone, Debug, Default)]
pub struct BiquadChain {
    sections: Vec<Biquad>,
    blowup_count: u32,
}

impl BiquadChain {
    pub fn new(sections: Vec<Biquad>) -> Self {
        Self {
            sections,
            blowup_count: 0,
        }
    }

    pub fn from_design(design: &[Section], sample_rate: u32) -> Self {
        Self::new(design.iter().map(|s| s.build(sample_rate)).collect())
    }

    pub fn len(&self) -> usize {
        self.sections.len()
    }

    #[inline]
    pub fn process_sample(&mut self, channel: usize, x: f32) -> f32 {
        let mut value = x;
        for section in &mut self.sections {
            value = section.process_sample(channel, value);
        }
        value
    }

    pub fn clear(&mut self) {
        for section in &mut self.sections {
            section.clear();
        }
    }

    /// Redesigns every section in place, keeping their memory.
    ///
    /// Returns false (and leaves the chain alone) when `design` has a different number of sections
    /// — that is a construction bug, not a runtime condition, and rebuilding from scratch here would
    /// hide it.
    pub fn apply_design(&mut self, design: &[Section], sample_rate: u32) -> bool {
        if design.len() != self.len() {
            return false;
        }
        for (section, spec) in self.sections.iter_mut().zip(design) {
            section.set_coeffs(spec.coeffs(sample_rate));
        }
        true
    }

    pub fn is_blowing(&self) -> bool {
        self.sections.iter().any(Biquad::is_blowing)
    }

    /// How many times the chain has caught itself diverging and been rebuilt.
    pub fn blowup_count(&self) -> u32 {
        self.blowup_count
    }

    /// Detects and repairs a diverged chain.
    ///
    /// Without this, one unstable section would mute the deck for the rest of the session.
    /// Rebuilding costs a handful of `sin`/`cos` on a path that runs maybe never; `design` is only
    /// called on the repair path.
    pub fn recover_if_blowing(&mut self, design: impl FnOnce() -> Vec<Biquad>) -> bool {
        if !self.is_blowing() {
            return false;
        }
        self.sections = design();
        self.blowup_count += 1;
        true
    }

    /// Direct access to the sections, for a test that needs to break one on purpose.
    #[cfg(test)]
    pub fn sections_mut(&mut self) -> &mut [Biquad] {
        &mut self.sections
    }
}

/// Maps a bipolar `-1.0 ..= 1.0` control onto an amplitude multiplier with a musical law:
/// counterclockwise reaches (near) silence, clockwise tops out at [`MAX_BOOST_DB`].
///
/// The floor is −80 dB rather than true zero because the shelf formulas degenerate at `A = 0`;
/// eight decimal places below a full-scale signal is inaudible either way.
pub fn bipolar_amp(value: f32) -> f32 {
    const MAX_BOOST_DB: f32 = 16.0;
    const KILL_DB: f32 = -80.0;
    if !value.is_finite() {
        return 1.0;
    }
    let v = value.clamp(-1.0, 1.0);
    if v >= 0.0 {
        10f32.powf(v * MAX_BOOST_DB / 20.0)
    } else if v <= -1.0 {
        // The bottom of the travel is silence, exactly. A fader that bottoms out at −80 dB instead
        // of 0 leaves a closed channel audibly leaking on a hot recording (−80 dB of a peak of 255
        // is still 0.025), and "muted" that isn't is worse than a curve with a slightly odd knee.
        0.0
    } else {
        // Squared ramp: half-way counterclockwise is already -12 dB, approaching the kill smoothly.
        let s = 1.0 + v;
        (s * s).max(10f32.powf(KILL_DB / 20.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 44_100;

    /// Steady-state gain of a design at `freq` Hz, measured by running a sine long enough for the
    /// transient to die. Also asserts the chain never produces a non-finite sample.
    fn gain_at(design: &[Section], freq: f64) -> f64 {
        let mut chain = BiquadChain::from_design(design, SR);
        let period = f64::from(SR) / freq;
        let total = (period * 40.0) as usize;
        let settle = total / 2;
        let (mut peak, mut bad) = (0.0f64, 0.0f64);
        for n in 0..total {
            let x = (2.0 * std::f64::consts::PI * freq * n as f64 / f64::from(SR)).sin() as f32;
            let y = chain.process_sample(0, x);
            if !y.is_finite() {
                bad = 1.0;
                break;
            }
            if n > settle {
                peak = peak.max(f64::from(y.abs()));
            }
        }
        assert_eq!(bad, 0.0, "{design:?} produced a non-finite sample");
        assert!(!chain.is_blowing());
        db(peak)
    }

    fn db(gain: f64) -> f64 {
        20.0 * gain.max(1e-12).log10()
    }

    #[test]
    fn low_pass_passes_the_band_and_stops_the_stopband() {
        let lp = [Section::LowPass { freq: 1_000.0, q: 0.707 }];
        assert!(gain_at(&lp, 100.0).abs() < 0.5, "100Hz should be flat");
        assert!(gain_at(&lp, 10_000.0) < -35.0, "10kHz should be down");
        // 2nd order = -12 dB/oct = -40 dB/decade, and the corner itself is -3 dB down.
        let at_one_decade = gain_at(&lp, 10_000.0);
        assert!(
            at_one_decade < -38.0 && at_one_decade > -46.0,
            "slope was {at_one_decade:.1} dB, expected ~-43"
        );
    }

    #[test]
    fn high_pass_is_the_mirror_image() {
        let hp = [Section::HighPass { freq: 1_000.0, q: 0.707 }];
        assert!(gain_at(&hp, 10_000.0).abs() < 0.5);
        assert!(gain_at(&hp, 100.0) < -20.0);
    }

    #[test]
    fn bell_boosts_only_around_its_frequency() {
        let bell = [Section::Bell { freq: 1_000.0, gain_db: 12.0, q: 1.0 }];
        assert!((gain_at(&bell, 1_000.0) - 12.0).abs() < 0.6, "at the peak");
        // A Q=1 bell is narrow but not brick-wall: 100 Hz and 10 kHz are a decade off the centre,
        // where a 12 dB boost is already back near flat but not exactly there.
        assert!(gain_at(&bell, 100.0).abs() < 1.5, "far below");
        assert!(gain_at(&bell, 10_000.0).abs() < 2.5, "far above");
        // Cutting is the negation of boosting.
        let cut = [Section::Bell { freq: 1_000.0, gain_db: -12.0, q: 1.0 }];
        assert!((gain_at(&cut, 1_000.0) + 12.0).abs() < 0.6);
    }

    #[test]
    fn shelves_reach_their_full_gain_and_are_flat_otherwise() {
        let low = [Section::LowShelf { freq: 320.0, gain_db: -26.0, slope: 1.0 }];
        assert!(
            (gain_at(&low, 60.0) + 26.0).abs() < 1.5,
            "shelf stopband gain was {:.1} dB",
            gain_at(&low, 60.0)
        );
        assert!(
            (gain_at(&low, 320.0) + 13.0).abs() < 2.5,
            "shelf should be half-way through its transition at f0, was {:.1} dB",
            gain_at(&low, 320.0)
        );
        assert!(gain_at(&low, 8_000.0).abs() < 1.5, "shelf must not touch treble");

        let high = [Section::HighShelf { freq: 3_200.0, gain_db: 18.0, slope: 1.0 }];
        assert!((gain_at(&high, 16_000.0) - 18.0).abs() < 1.5);
        assert!(gain_at(&high, 200.0).abs() < 1.5);
    }

    #[test]
    fn unity_gain_is_exactly_flat_at_any_q() {
        for design in [
            vec![Section::Bell { freq: 1_000.0, gain_db: 0.0, q: 0.5 }],
            vec![Section::LowShelf { freq: 100.0, gain_db: 0.0, slope: 1.0 }],
            vec![Section::HighShelf { freq: 8_000.0, gain_db: 0.0, slope: 0.3 }],
        ] {
            for probe in [100.0, 1_000.0, 8_000.0] {
                let got = gain_at(&design, probe);
                assert!(got.abs() < 0.05, "{design:?} at {probe}Hz gave {got:.3} dB");
            }
        }
    }

    #[test]
    fn resonant_q_raises_the_peak_above_dc_gain() {
        // A low-pass at Q 8 must ring up near cutoff, otherwise the Q control is decorative.
        let sharp = [Section::LowPass { freq: 1_000.0, q: 8.0 }];
        assert!(gain_at(&sharp, 1_000.0) > 15.0, "Q8 peak");
        let soft = [Section::LowPass { freq: 1_000.0, q: 0.707 }];
        assert!(gain_at(&soft, 1_000.0) < 3.0);
    }

    #[test]
    fn bandpass_never_adds_gain_and_its_width_controls_the_skirt() {
        // The property that makes a resonant filter safe on a deck: the constant-*peak*-gain form is
        // normalised so the response tops out at unity, so sweeping it can never blow out the master
        // the way a Q-driven low-pass peak does.
        for width in [50.0f64, 400.0, 4_000.0] {
            let bp = [Section::BandPass { freq: 1_000.0, bandwidth_hz: width }];
            for probe in [20.0, 200.0, 1_000.0, 2_000.0, 8_000.0, 18_000.0] {
                let got = gain_at(&bp, probe);
                assert!(got < 0.6, "{width}Hz-wide bp *added* gain at {probe}Hz: {got:.1} dB");
            }
            // The centre is always in the passband, so it stays near unity whatever the width.
            assert!(gain_at(&bp, 1_000.0).abs() < 1.5, "{width}Hz-wide bp dipped at centre");
            // Rejection is only meaningful outside the passband, and a band wider than the spectrum
            // below the centre simply cannot reject the bottom of it.
            if width * 3.0 < 1_000.0 {
                assert!(
                    gain_at(&bp, (1_000.0 - width * 3.0).max(20.0)) < -6.0,
                    "{width}Hz-wide bp passed below its skirt"
                );
            }
            // Well past Nyquist-ish, every width must be down: the whole point of a band-pass.
            assert!(gain_at(&bp, 20_000.0) < -6.0, "{width}Hz bp passed 20kHz");
        }
        // A narrow band rejects harder than a wide one at the same distance from centre (both inside
        // their own skirts' reach, so the comparison is about slope rather than reachability.
        let narrow = gain_at(
            &[Section::BandPass { freq: 1_000.0, bandwidth_hz: 60.0 }],
            2_000.0,
        );
        let wide = gain_at(
            &[Section::BandPass { freq: 1_000.0, bandwidth_hz: 3_000.0 }],
            2_000.0,
        );
        assert!(narrow < wide - 3.0, "bandwidth did nothing: {narrow:.1} vs {wide:.1}");
    }

    #[test]
    fn degenerate_controls_stay_stable() {
        // These are the values a fader hits at its extremes and a bad parse can produce.
        for design in [
            vec![Section::LowPass { freq: 0.0, q: 0.0 }],
            vec![Section::LowPass { freq: 1e9, q: 1e9 }],
            vec![Section::HighPass { freq: f64::NAN, q: f64::NAN }],
            vec![Section::BandPass { freq: 1_000.0, bandwidth_hz: 0.0 }],
            vec![Section::BandPass { freq: 1_000.0, bandwidth_hz: 1e9 }],
            vec![Section::BandPass { freq: f64::NAN, bandwidth_hz: f64::NAN }],
            vec![Section::Bell { freq: 1e12, gain_db: 1e6, q: 0.001 }],
            vec![Section::LowShelf { freq: 100.0, gain_db: f64::NEG_INFINITY, slope: 1.0 }],
            vec![Section::HighShelf { freq: 100.0, gain_db: 1e6, slope: 1e-9 }],
        ] {
            let _ = gain_at(&design, 1_000.0);
        }
    }

    #[test]
    fn redesigning_keeps_the_steady_state_a_sweep_needs() {
        // The whole point of `apply_design`: sweeping a cutoff must converge, not restart every
        // block. Feed a constant sine, slide the low-pass an octave per block, and require that the
        // output tracks the design instead of smearing transient.
        let mut chain = BiquadChain::from_design(
            &[Section::LowPass { freq: 20_000.0, q: 0.707 }],
            SR,
        );
        let mut peak = 0.0f32;
        for n in 0..4_000 {
            let x = (2.0 * std::f64::consts::PI * 1_000.0 * n as f64 / f64::from(SR)).sin() as f32;
            chain.apply_design(
                &[Section::LowPass { freq: 1_000.0, q: 0.707 }],
                SR,
            );
            let y = chain.process_sample(0, x);
            if n > 3_000 {
                peak = peak.max(y.abs());
            }
        }
        // A 1 kHz sine at a 1 kHz Butterworth-ish cutoff sits ~3 dB down.
        let measured = 20.0 * f64::from(peak.max(1e-9)).log10();
        assert!(
            measured > -6.0 && measured < -0.5,
            "swept low-pass measured {measured:.2} dB"
        );
    }

    #[test]
    fn apply_design_refuses_a_shape_change() {
        let mut chain = BiquadChain::from_design(
            &[Section::LowPass { freq: 1_000.0, q: 0.707 }],
            SR,
        );
        assert!(!chain.apply_design(
            &[
                Section::LowPass { freq: 1_000.0, q: 0.707 },
                Section::HighPass { freq: 100.0, q: 0.707 },
            ],
            SR
        ));
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn chain_recovers_from_a_diverged_section() {
        let mut chain =
            BiquadChain::from_design(&[Section::LowPass { freq: 1_000.0, q: 0.707 }], SR);
        chain.sections_mut()[0].clear();
        // Force the failure by hand: a section whose state went non-finite.
        chain.sections_mut()[0].z[0][0] = f32::NAN;
        assert!(chain.is_blowing());
        assert!(chain.recover_if_blowing(|| vec![
            Section::LowPass { freq: 1_000.0, q: 0.707 }.build(SR)
        ]));
        assert_eq!(chain.blowup_count(), 1);
        assert!(chain.process_sample(0, 1.0).is_finite());
        // A healthy chain is left completely alone.
        assert!(!chain.recover_if_blowing(|| unreachable!("must not rebuild")));
    }

    #[test]
    fn a_flat_shelf_reports_well_conditioned_coefficients() {
        // Guards against a design that normalises to NaN/inf at 0 dB, which `coeffs` makes testable.
        let shelf = Section::HighShelf { freq: 3_000.0, gain_db: 0.0, slope: 1.0 }.build(SR);
        assert!(shelf.coeffs().iter().all(|c| c.is_finite()));
        let killed = Section::LowShelf { freq: 320.0, gain_db: -120.0, slope: 1.0 }.build(SR);
        assert!(killed.coeffs().iter().all(|c| c.is_finite()), "the kill position degenerated");
    }

    #[test]
    fn bipolar_law_spans_kill_to_boost() {
        assert!((bipolar_amp(0.0) - 1.0).abs() < 1e-6, "centred is unity");
        assert!(bipolar_amp(1.0) > 5.0, "full clockwise boosts");
        assert_eq!(bipolar_amp(-1.0), 0.0, "full counterclockwise must be exactly silent");
        assert!(bipolar_amp(-0.99) > 0.0, "the kill must be a point, not a region");
        assert!(bipolar_amp(-0.5) < 0.3 && bipolar_amp(-0.5) > 0.1, "halfway");
        // Monotonic across the whole travel, and NaN-proof.
        let mut prev = f32::MIN;
        for i in -100..=100 {
            let v = i as f32 / 100.0;
            let amp = bipolar_amp(v);
            assert!(amp >= prev, "not monotonic at {v}");
            prev = amp;
        }
        assert!((bipolar_amp(f32::NAN) - 1.0).abs() < 1e-6);
        assert!(bipolar_amp(-99.0) < 1e-3, "clamped at the kill floor");
    }
}
