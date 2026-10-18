//! Media layer: turn files into PCM and expose them as a [`Source`].
//!
//! Three things live here: the whole-file decoder, the in-memory pool that playback and analysis
//! read from, and the growing source (`ChunkedStreams`) that anything produced in blocks — a
//! progressive separation today — installs into a deck. It knows nothing about stems: it grows
//! `n` streams and hands them out.
//! No timing, no transport — those are `audio`'s job.

mod chunked;
mod decoder;
mod pool;

pub use chunked::{ChunkedStreams, DEFAULT_FADE_FRAMES};
pub use decoder::{decode_file, decode_file_at, DecodedAudio};
pub use pool::PcmPool;

// The `Source` trait is defined in `core` (the shared protocol) but re-exported here so media
// consumers can `use hypermixx_media::Source` without naming core directly.
pub use hypermixx_core::{Shared, Source};
