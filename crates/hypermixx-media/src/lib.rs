//! Media layer: turn files into PCM and expose them as a [`Source`].
//!
//! Only two things live here: the whole-file decoder and the in-memory pool that playback and
//! analysis read from. No timing, no transport — those are `audio`'s job.

mod decoder;
mod pool;

pub use decoder::{decode_file, DecodedAudio};
pub use pool::PcmPool;

// The `Source` trait is defined in `core` (the shared protocol) but re-exported here so media
// consumers can `use hypermixx_media::Source` without naming core directly.
pub use hypermixx_core::{Shared, Source};
