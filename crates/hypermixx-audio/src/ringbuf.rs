//! Lock-free f32 ring buffer between the producer thread and the audio callback.

use rtrb::{Consumer, Producer};

use crate::CHANNELS;

/// SPSC ring buffer sized in frames (one frame = [`CHANNELS`] samples).
///
/// rtrb handles are single-owner (not `Clone`), so the two ends are handed to their threads with
/// [`split`](Self::split) rather than cloned — that keeps the audio callback lock-free.
pub struct AudioRingBuffer {
    producer: Producer<f32>,
    consumer: Consumer<f32>,
}

impl AudioRingBuffer {
    pub fn new(capacity_frames: usize) -> Self {
        let (producer, consumer) = rtrb::RingBuffer::new(capacity_frames * CHANNELS);
        Self { producer, consumer }
    }

    /// Writes samples, returning how many fit. Overflow is dropped, never blocked on.
    pub fn push(&mut self, data: &[f32]) -> usize {
        push_samples(&mut self.producer, data)
    }

    /// Reads samples into `output`, returning how many were available.
    pub fn pop(&mut self, output: &mut [f32]) -> usize {
        pop_samples(&mut self.consumer, output)
    }

    /// Samples currently readable.
    pub fn available(&self) -> usize {
        self.consumer.slots()
    }

    /// Frees the two ends so each thread can own one without a lock.
    pub fn split(self) -> (Producer<f32>, Consumer<f32>) {
        (self.producer, self.consumer)
    }
}

/// Producer-side push. Usable from the producer thread with an owned [`Producer`].
pub fn push_samples(producer: &mut Producer<f32>, data: &[f32]) -> usize {
    let (written, _remainder) = producer.push_partial_slice(data);
    written.len()
}

/// Consumer-side pop. Usable from the audio callback with an owned [`Consumer`].
pub fn pop_samples(consumer: &mut Consumer<f32>, output: &mut [f32]) -> usize {
    let (read, _remainder) = consumer.pop_partial_slice(output);
    read.len()
}

/// Fills `output` from the ring, padding an underrun with silence. Audio-callback safe.
pub fn fill_with_silence_on_underrun(consumer: &mut Consumer<f32>, output: &mut [f32]) {
    let read = pop_samples(consumer, output);
    output[read..].fill(0.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_samples() {
        let mut ring = AudioRingBuffer::new(4);
        let data = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(ring.push(&data), 4);
        assert_eq!(ring.available(), 4);
        let mut out = [0.0f32; 4];
        assert_eq!(ring.pop(&mut out), 4);
        assert_eq!(out, data);
        assert_eq!(ring.available(), 0);
    }

    #[test]
    fn overflow_is_dropped_not_blocked() {
        let mut ring = AudioRingBuffer::new(1); // capacity: 2 samples
        let data = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        assert_eq!(ring.push(&data), 2);
        assert_eq!(ring.available(), 2);
    }

    #[test]
    fn underrun_padding_is_silent() {
        let mut ring = AudioRingBuffer::new(4);
        ring.push(&[1.0, 2.0]);
        let (mut producer, mut consumer) = ring.split();
        let mut out = [7.0f32; 8];
        fill_with_silence_on_underrun(&mut consumer, &mut out);
        assert_eq!(&out[..2], &[1.0, 2.0]);
        assert!(out[2..].iter().all(|s| *s == 0.0));
        assert_eq!(push_samples(&mut producer, &[3.0, 4.0]), 2);
        assert_eq!(pop_samples(&mut consumer, &mut out[..4]), 2);
        assert_eq!(&out[..2], &[3.0, 4.0]);
    }
}
