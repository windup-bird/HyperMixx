//! A source that grows: blocks arrive while the audio thread is reading it.
//!
//! Whatever it is growing *into* is the caller's business — separated stems arriving from a
//! separator, a track being fetched, a mix being decoded on the fly. What lives here is only the
//! mechanism, and it has exactly two rules, because the reader is the audio thread and a short read
//! there is silence:
//!
//! * **Reads never block, and never see a half-block.** Available audio sits behind an
//!   [`ArcSwap`] snapshot that is replaced on every append (the `Arc`s are cloned, not the samples),
//!   so a reader works from immutable data with no lock of any kind.
//! * **Reads past what exists still return audio**, if the caller gave a [`fallback`] — for stems
//!   that is the mix itself on stream 0 with the others silent, which keeps the *sum* unchanged, and
//!   a short cross-fade into it means the moment production catches up is not a step.
//!
//! `total_frames` is the whole thing, not the watermark: the transport, the waveform and the beat
//! grid describe the finished item, and a reader deciding "have I run out?" must be told the truth.
//!
//! [`fallback`]: ChunkedStreams::new

use std::sync::Arc;

use arc_swap::ArcSwap;
use hypermixx_core::{Source, Shared, CHANNELS};

/// Cross-fade at the frontier, in frames: ~50 ms at the engine rate. Long enough that the blend is
/// inaudible, short enough that it is over before a listener notices the fallback handing over.
pub const DEFAULT_FADE_FRAMES: u64 = 2205;

/// One appended block: `frames` frames per stream, contiguous, starting at `start`.
#[derive(Clone)]
struct Block {
    start: u64,
    frames: usize,
    /// One interleaved buffer per stream (`frames * CHANNELS` samples each).
    pcm: Vec<Arc<Vec<f32>>>,
}

/// The immutable view a reader works from. Replaced whole on every append.
struct Snapshot {
    blocks: Vec<Block>,
    /// Frames `0..watermark` exist (and are contiguous).
    watermark: u64,
}

impl Snapshot {
    /// Copies frames `start..start+frames` of one stream, zero-filling anything not appended yet.
    fn read(&self, stream: usize, start: u64, frames: usize, out: &mut [f32]) {
        out[..frames * CHANNELS].fill(0.0);
        for block in &self.blocks {
            let Some(samples) = block.pcm.get(stream) else {
                continue;
            };
            let end = block.start + block.frames as u64;
            // The overlap of `[start, start+frames)` with this block.
            let from = start.max(block.start);
            let to = (start + frames as u64).min(end);
            if from >= to {
                continue;
            }
            let src_off = ((from - block.start) as usize) * CHANNELS;
            let dst_off = ((from - start) as usize) * CHANNELS;
            let len = ((to - from) as usize) * CHANNELS;
            out[dst_off..dst_off + len].copy_from_slice(&samples[src_off..src_off + len]);
        }
    }

    /// One frame of one stream, zero if it has not been appended.
    fn sample(&self, stream: usize, frame: u64) -> [f32; CHANNELS] {
        for block in &self.blocks {
            let end = block.start + block.frames as u64;
            if frame < block.start || frame >= end {
                continue;
            }
            let Some(samples) = block.pcm.get(stream) else {
                break;
            };
            let off = ((frame - block.start) as usize) * CHANNELS;
            return [samples[off], samples[off + 1]];
        }
        [0.0, 0.0]
    }
}

struct Inner {
    /// The whole thing, watermark or not.
    total: u64,
    streams: usize,
    fade: u64,
    /// What plays before production has caught up, on stream 0 with the rest silent. `None` means
    /// silence beyond the watermark — right for a source that is simply being produced and was never
    /// playing anything else.
    fallback: Option<Shared>,
    snapshot: ArcSwap<Snapshot>,
}

impl Inner {
    /// Reads `frames` of the fallback, zero-padded if it is shorter than the total claims.
    fn read_fallback(&self, start: u64, frames: usize, out: &mut [f32]) -> usize {
        let samples = frames * CHANNELS;
        let Some(fallback) = self.fallback.as_ref() else {
            out[..samples].fill(0.0);
            return frames;
        };
        let got = fallback.read_frames(start, &mut out[..samples]);
        out[got * CHANNELS..samples].fill(0.0);
        got
    }
}

/// A growing multi-stream source. Cheap to clone; the streams it hands out share one state.
pub struct ChunkedStreams {
    inner: Arc<Inner>,
}

impl ChunkedStreams {
    /// `streams` streams over a total of `total` frames, playing `fallback` wherever production has
    /// not reached. `None` for a source that has nothing else to play — silence then, with the same
    /// short cross-fade at the frontier.
    pub fn new(streams: usize, total: u64, fallback: Option<Shared>, fade_frames: u64) -> Self {
        assert!(streams > 0, "a source needs at least one stream");
        Self {
            inner: Arc::new(Inner {
                total,
                streams,
                fade: fade_frames,
                fallback,
                snapshot: ArcSwap::from_pointee(Snapshot {
                    blocks: Vec::new(),
                    watermark: 0,
                }),
            }),
        }
    }

    /// The `Source` handles to install. They never need replacing: a later
    /// [`append`](Self::append) is visible through them.
    pub fn streams(&self) -> Vec<Shared> {
        (0..self.inner.streams)
            .map(|index| {
                Arc::new(StreamView {
                    inner: Arc::clone(&self.inner),
                    index,
                }) as Shared
            })
            .collect()
    }

    /// Appends one block: one interleaved buffer per stream, all the same length.
    ///
    /// The watermark only moves over a *contiguous* run from frame 0, so a block that arrived out of
    /// order is stored and readable but does not pretend the gap before it exists.
    pub fn append(&self, start: u64, block: Vec<Arc<Vec<f32>>>) -> Result<(), String> {
        if block.len() != self.inner.streams {
            return Err(format!(
                "a block needs {} streams, got {}",
                self.inner.streams,
                block.len()
            ));
        }
        let Some(first) = block.first() else {
            return Ok(());
        };
        if first.len() % CHANNELS != 0 {
            return Err(format!(
                "a block must be a whole number of stereo frames ({} samples)",
                first.len()
            ));
        }
        let frames = first.len() / CHANNELS;
        if block.iter().any(|stream| stream.len() != first.len()) {
            return Err("a block's streams must be frame-aligned".to_owned());
        }
        let current = self.inner.snapshot.load();
        let mut blocks = current.blocks.clone();
        blocks.push(Block {
            start,
            frames,
            pcm: block,
        });
        let watermark = watermark_of(&blocks);
        self.inner
            .snapshot
            .store(Arc::new(Snapshot { blocks, watermark }));
        Ok(())
    }

    /// How much of the total exists, in frames.
    pub fn available_frames(&self) -> u64 {
        self.inner.snapshot.load().watermark
    }

    /// The total length (not the watermark).
    pub fn total_frames(&self) -> u64 {
        self.inner.total
    }
}

/// The end of the contiguous run of blocks starting at frame 0.
fn watermark_of(blocks: &[Block]) -> u64 {
    let mut ranges: Vec<(u64, u64)> = blocks
        .iter()
        .map(|block| (block.start, block.start + block.frames as u64))
        .collect();
    ranges.sort_unstable();
    let mut watermark = 0;
    for (start, end) in ranges {
        if start > watermark {
            break; // a gap: nothing beyond it exists, and the list is sorted
        }
        watermark = watermark.max(end);
    }
    watermark
}

/// One stream of a [`ChunkedStreams`]: the `Source` a caller holds.
struct StreamView {
    inner: Arc<Inner>,
    index: usize,
}

impl Source for StreamView {
    fn read_frames(&self, start: u64, out: &mut [f32]) -> usize {
        if out.len() < CHANNELS {
            return 0;
        }
        let frames = (out.len() / CHANNELS).min(self.inner.total.saturating_sub(start) as usize);
        if frames == 0 {
            return 0;
        }
        let snapshot = self.inner.snapshot.load();
        let watermark = snapshot.watermark;
        if start >= watermark {
            // Entirely past what exists: stream 0 carries the fallback, the others silence, so the
            // streams still sum to whatever was playing before.
            if self.index == 0 {
                let _ = self.inner.read_fallback(start, frames, out);
            } else {
                out[..frames * CHANNELS].fill(0.0);
            }
            return frames;
        }
        let fade = self.inner.fade.min(watermark);
        let fade_start = watermark - fade;
        if start + frames as u64 <= fade_start {
            // Entirely inside what exists: a plain copy.
            snapshot.read(self.index, start, frames, out);
            return frames;
        }
        // The frontier itself: blend the produced audio into the fallback, keeping what the streams
        // *sum to* continuous, so production catching up is not a step.
        for i in 0..frames {
            let frame = start + i as u64;
            let blend = if fade > 0 && frame >= fade_start {
                ((frame - fade_start + 1) as f32 / (fade + 1) as f32).clamp(0.0, 1.0)
            } else if frame >= watermark {
                1.0
            } else {
                0.0
            };
            let produced = snapshot.sample(self.index, frame);
            // The fallback lives on stream 0: it blends from *its own* stream into the fallback while
            // the others fade out.
            if self.index == 0 {
                let mut fallback = [0.0f32; CHANNELS];
                self.inner.read_fallback(frame, 1, &mut fallback);
                let off = i * CHANNELS;
                out[off] = produced[0] + (fallback[0] - produced[0]) * blend;
                out[off + 1] = produced[1] + (fallback[1] - produced[1]) * blend;
            } else {
                let off = i * CHANNELS;
                out[off] = produced[0] * (1.0 - blend);
                out[off + 1] = produced[1] * (1.0 - blend);
            }
        }
        frames
    }

    fn total_frames(&self) -> u64 {
        self.inner.total
    }

    fn available_frames(&self) -> u64 {
        self.inner.snapshot.load().watermark
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::PcmPool;
    use crate::DecodedAudio;

    fn pool(frames: u64, value: f32) -> Shared {
        Arc::new(PcmPool::from_decoded(DecodedAudio {
            pcm: (0..frames as usize)
                .flat_map(|_| [value, -value])
                .collect(),
            total_frames: frames,
            sample_rate: hypermixx_core::SAMPLE_RATE,
            channels: CHANNELS,
        }))
    }

    /// A block of `frames` frames whose stream `i` carries `values[i]`.
    fn block(frames: usize, values: [f32; 4]) -> Vec<Arc<Vec<f32>>> {
        values
            .iter()
            .map(|value| Arc::new(vec![*value; frames * CHANNELS]))
            .collect()
    }

    fn read(stream: &Shared, start: u64, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; frames * CHANNELS];
        let got = stream.read_frames(start, &mut out);
        assert_eq!(got, frames, "a short read is a gap on the audio thread");
        out
    }

    /// Before anything arrives, the streams *are* the fallback: stream 0 carries it and the rest are
    /// silent, so the sum is exactly what was playing.
    #[test]
    fn before_the_first_block_the_streams_are_the_fallback() {
        let fallback = pool(10_000, 0.5);
        let growing = ChunkedStreams::new(
            4,
            10_000,
            Some(Arc::clone(&fallback)),
            DEFAULT_FADE_FRAMES,
        );
        let streams = growing.streams();
        assert_eq!(growing.available_frames(), 0);

        // The pool carries +0.5 on the left and −0.5 on the right, so check both planes.
        let first = read(&streams[0], 1_000, 256);
        assert!((first[0] - 0.5).abs() < 1e-6 && (first[1] + 0.5).abs() < 1e-6, "stream 0 = fallback");
        for stream in &streams[1..] {
            let silent = read(stream, 1_000, 256);
            assert!(silent.iter().all(|s| *s == 0.0), "the others are silent");
        }
        // The length a reader sees is the whole thing, so playback does not stop at the watermark.
        assert_eq!(streams[0].total_frames(), 10_000);
        assert_eq!(streams[0].available_frames(), 0);
    }

    /// With no fallback, what does not exist yet is silence — the right answer for a source that was
    /// never playing anything else.
    #[test]
    fn without_a_fallback_the_unproduced_part_is_silence() {
        let growing = ChunkedStreams::new(1, 10_000, None, DEFAULT_FADE_FRAMES);
        let streams = growing.streams();
        assert_eq!(streams.len(), 1, "one stream is all a non-stem user needs");
        assert!(read(&streams[0], 0, 128).iter().all(|s| *s == 0.0));
        // Long enough that the cross-fade (2 205 frames) does not cover the whole block: a block
        // shorter than the fade is a fade-in from the fallback, which is right but not what this
        // test is about.
        growing
            .append(0, vec![Arc::new(vec![0.25f32; 5_000 * CHANNELS])])
            .unwrap();
        assert!((read(&streams[0], 100, 64)[0] - 0.25).abs() < 1e-6);
        assert!(read(&streams[0], 5_000, 64).iter().all(|s| *s == 0.0), "silence, not a gap");
    }

    /// A block is audible *through the handles the caller already holds* — nothing is reinstalled.
    #[test]
    fn an_appended_block_is_visible_through_the_existing_handles() {
        let fallback = pool(10_000, 0.5);
        let growing = ChunkedStreams::new(4, 10_000, Some(fallback), DEFAULT_FADE_FRAMES);
        let streams = growing.streams();
        growing.append(0, block(4_000, [1.0, 2.0, 3.0, 4.0])).unwrap();

        assert_eq!(growing.available_frames(), 4_000);
        assert_eq!(streams[0].available_frames(), 4_000);
        // Well inside what exists (the fade only touches the last few ms before the frontier).
        assert_eq!(read(&streams[3], 100, 64)[0], 4.0);
        // Past it: back to the fallback on stream 0, silence elsewhere.
        assert!((read(&streams[0], 5_000, 64)[0] - 0.5).abs() < 1e-6);
        assert_eq!(read(&streams[1], 5_000, 64)[0], 0.0);
    }

    /// At the frontier the streams still *sum* to the fallback, so production catching up is not a
    /// jump: the blend is continuous at both ends.
    #[test]
    fn the_frontier_cross_fades_without_changing_the_sum() {
        let fallback = pool(10_000, 0.5);
        let growing = ChunkedStreams::new(4, 10_000, Some(Arc::clone(&fallback)), 100);
        let streams = growing.streams();
        // A block whose four streams sum to 0.5 (the fallback): 0.1 + 0.1 + 0.1 + 0.2.
        growing.append(0, block(1_000, [0.1, 0.1, 0.1, 0.2])).unwrap();

        let sum = |start: u64| -> f32 {
            streams
                .iter()
                .map(|stream| read(stream, start, 1)[0])
                .sum()
        };
        assert!((sum(500) - 0.5).abs() < 1e-5);
        for start in 940..1_000 {
            let value = sum(start);
            assert!((value - 0.5).abs() < 1e-4, "the sum moved at {start}: {value}");
        }
        assert!((sum(1_500) - 0.5).abs() < 1e-5);

        // With a residual (the streams do *not* sum to the fallback) the sum travels smoothly from
        // one to the other rather than stepping.
        let fallback = pool(10_000, 0.5);
        let growing = ChunkedStreams::new(4, 10_000, Some(fallback), 100);
        let streams = growing.streams();
        growing.append(0, block(1_000, [0.1, 0.1, 0.1, 0.1])).unwrap();
        let sum = |start: u64| -> f32 {
            streams
                .iter()
                .map(|stream| read(stream, start, 1)[0])
                .sum()
        };
        assert!((sum(800) - 0.4).abs() < 1e-5, "before the fade it is the streams");
        assert!((sum(1_000) - 0.5).abs() < 1e-5, "at the frontier it is the fallback");
        let mut previous = sum(899);
        for start in 900..=1_000 {
            let value = sum(start);
            assert!(
                (value - previous).abs() < 0.02,
                "the sum stepped {previous} -> {value} at {start}"
            );
            previous = value;
        }
    }

    /// A block that does not line up with the watermark is stored but not trusted.
    #[test]
    fn only_a_contiguous_block_moves_the_watermark() {
        let growing = ChunkedStreams::new(4, 10_000, None, DEFAULT_FADE_FRAMES);
        growing.append(4_000, block(1_000, [1.0; 4])).unwrap();
        assert_eq!(growing.available_frames(), 0, "a gap does not exist yet");
        growing.append(0, block(4_000, [1.0; 4])).unwrap();
        assert_eq!(growing.available_frames(), 5_000, "the gap was filled in");
        growing.append(5_000, block(1_000, [1.0; 4])).unwrap();
        assert_eq!(growing.available_frames(), 6_000);
    }

    /// Malformed blocks are refused where they are made, not on the audio thread.
    #[test]
    fn a_malformed_block_is_refused() {
        let growing = ChunkedStreams::new(4, 1_000, None, DEFAULT_FADE_FRAMES);
        let two = vec![Arc::new(vec![1.0f32; 20]), Arc::new(vec![1.0f32; 20])];
        assert!(growing.append(0, two).is_err(), "wrong stream count");
        let ragged = vec![
            Arc::new(vec![1.0f32; 20]),
            Arc::new(vec![1.0f32; 10]),
            Arc::new(vec![1.0f32; 20]),
            Arc::new(vec![1.0f32; 20]),
        ];
        assert!(growing.append(0, ragged).is_err(), "streams must be frame-aligned");
        let odd = vec![
            Arc::new(vec![1.0f32; 21]),
            Arc::new(vec![1.0f32; 21]),
            Arc::new(vec![1.0f32; 21]),
            Arc::new(vec![1.0f32; 21]),
        ];
        assert!(growing.append(0, odd).is_err(), "a whole number of stereo frames");
        assert_eq!(growing.available_frames(), 0, "nothing was stored");
    }
}
