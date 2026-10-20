//! A signed-rate, cubic varispeed reader for vinyl scratch.
//!
//! Unlike the keylock `Playing`, this is not a time-stretcher: it simply walks the source at a
//! signed rate (source frames per output frame), so pitch follows speed and reverse is just a
//! negative rate. It is the Tape path with a moving hand: zero FFT, no seek-per-block.

use std::sync::Arc;

use crate::mixer::Bus;
use crate::CHANNELS;
use hypermixx_core::Source;

/// Largest scratch speed, in source frames per output frame.
pub const MAX_SCRATCH_RATE: f64 = 4.0;

pub struct Scratch {
    sources: Vec<Arc<dyn Source>>,
    /// Source position (fractional) of the next output frame.
    position: f64,
    /// Source frames per output frame; negative plays backwards.
    rate: f64,
    total: u64,
    stem_buf: Vec<f32>,
}

impl Scratch {
    pub fn new(sources: Vec<Arc<dyn Source>>, start: u64) -> Self {
        assert!(!sources.is_empty(), "scratch needs a source");
        let total = sources[0].total_frames();
        Self {
            sources,
            position: start.min(total.saturating_sub(1)) as f64,
            rate: 0.0,
            total,
            stem_buf: vec![0.0; 4096 * CHANNELS],
        }
    }

    pub fn stream_count(&self) -> usize {
        self.sources.len()
    }

    pub fn position(&self) -> u64 {
        self.position.round().clamp(0.0, self.total.saturating_sub(1) as f64) as u64
    }

    #[cfg(test)]
    pub fn seek(&mut self, position: u64) {
        self.position = position.min(self.total.saturating_sub(1)) as f64;
        self.rate = 0.0;
    }

    pub fn set_rate(&mut self, rate: f64) {
        self.rate = if rate.is_finite() {
            rate.clamp(-MAX_SCRATCH_RATE, MAX_SCRATCH_RATE)
        } else {
            0.0
        };
    }

    pub fn process_streams(&mut self, outs: &mut [Bus], frames: usize) -> usize {
        let streams = outs.len().min(self.sources.len());
        let frames = frames.min(outs.iter().map(Bus::frames).min().unwrap_or(0));
        if frames == 0 || streams == 0 {
            return frames;
        }
        if self.rate == 0.0 {
            for bus in outs.iter_mut().take(streams) {
                bus.l[..frames].fill(0.0);
                bus.r[..frames].fill(0.0);
            }
            return frames;
        }
        let (lo, window) = self.window(frames);
        if window <= 3 {
            for bus in outs.iter_mut().take(streams) {
                bus.l[..frames].fill(0.0);
                bus.r[..frames].fill(0.0);
            }
            self.advance(frames);
            return frames;
        }
        if self.stem_buf.len() < window * CHANNELS {
            self.stem_buf.resize(window * CHANNELS, 0.0);
        }
        for (s, bus) in outs.iter_mut().take(streams).enumerate() {
            self.read_window(s, lo, window);
            for i in 0..frames {
                let rel = self.position + i as f64 * self.rate - lo as f64;
                let (l, r) = interpolate(&self.stem_buf[..window * CHANNELS], window, rel);
                bus.l[i] = l;
                bus.r[i] = r;
            }
        }
        self.advance(frames);
        frames
    }

    pub fn process_block(&mut self, output: &mut [f32]) -> usize {
        let frames = output.len() / CHANNELS;
        output[..frames * CHANNELS].fill(0.0);
        if frames == 0 || self.rate == 0.0 {
            return frames;
        }
        let (lo, window) = self.window(frames);
        if window <= 3 {
            self.advance(frames);
            return frames;
        }
        if self.stem_buf.len() < window * CHANNELS {
            self.stem_buf.resize(window * CHANNELS, 0.0);
        }
        for s in 0..self.sources.len() {
            self.read_window(s, lo, window);
            for i in 0..frames {
                let rel = self.position + i as f64 * self.rate - lo as f64;
                let (l, r) = interpolate(&self.stem_buf[..window * CHANNELS], window, rel);
                output[i * CHANNELS] += l;
                output[i * CHANNELS + 1] += r;
            }
        }
        self.advance(frames);
        frames
    }

    fn window(&self, frames: usize) -> (u64, usize) {
        let start = self.position;
        let end = self.position + self.rate * frames as f64;
        let min = start.min(end);
        let max = start.max(end);
        let lo = (min.floor() as i64 - 1).max(0) as u64;
        let hi = (max.ceil() as i64 + 2).min(self.total as i64).max(0) as u64;
        (lo, hi.saturating_sub(lo) as usize)
    }

    fn read_window(&mut self, stream: usize, lo: u64, window: usize) {
        let got = self.sources[stream].read_frames(lo, &mut self.stem_buf[..window * CHANNELS]);
        if got < window {
            self.stem_buf[got * CHANNELS..window * CHANNELS].fill(0.0);
        }
    }

    fn advance(&mut self, frames: usize) {
        self.position += self.rate * frames as f64;
        let last = self.total.saturating_sub(1) as f64;
        if self.position < 0.0 {
            self.position = 0.0;
            self.rate = 0.0;
        } else if self.position > last {
            self.position = last;
            self.rate = 0.0;
        }
    }
}

/// Cubic Hermite interpolation at a fractional index. `rel` is relative to the window start.
fn interpolate(buf: &[f32], window: usize, rel: f64) -> (f32, f32) {
    let idx = rel.floor() as isize;
    let frac = (rel - idx as f64) as f32;
    let i0 = (idx - 1).clamp(0, window as isize - 1) as usize;
    let i1 = idx.clamp(0, window as isize - 1) as usize;
    let i2 = (idx + 1).clamp(0, window as isize - 1) as usize;
    let i3 = (idx + 2).clamp(0, window as isize - 1) as usize;
    let channel = |i: usize, c: usize| buf[i * CHANNELS + c];
    let cubic = |s0: f32, s1: f32, s2: f32, s3: f32| {
        let c0 = s1;
        let c1 = 0.5 * (s2 - s0);
        let c2 = s0 - 2.5 * s1 + 2.0 * s2 - 0.5 * s3;
        let c3 = 0.5 * (s3 - s0) + 1.5 * (s1 - s2);
        ((c3 * frac + c2) * frac + c1) * frac + c0
    };
    (
        cubic(channel(i0, 0), channel(i1, 0), channel(i2, 0), channel(i3, 0)),
        cubic(channel(i0, 1), channel(i1, 1), channel(i2, 1), channel(i3, 1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_media::{DecodedAudio, PcmPool};

    fn ramp(n: u64) -> Arc<dyn Source> {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..n as usize)
                .flat_map(|i| [i as f32, -(i as f32)])
                .collect(),
            total_frames: n,
            sample_rate: crate::SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    #[test]
    fn positive_rate_walks_forward_and_negative_walks_back() {
        let mut s = Scratch::new(vec![ramp(10_000)], 1000);
        s.set_rate(1.0);
        let mut bus = Bus::stereo(8);
        s.process_streams(std::slice::from_mut(&mut bus), 8);
        assert!((bus.l[0] - 1000.0).abs() < 1e-3);
        assert!((bus.l[7] - 1007.0).abs() < 1e-3);

        s.seek(1000);
        s.set_rate(-1.0);
        s.process_streams(std::slice::from_mut(&mut bus), 8);
        assert!((bus.l[0] - 1000.0).abs() < 1e-3);
        assert!((bus.l[7] - 993.0).abs() < 1e-3);
    }
}
