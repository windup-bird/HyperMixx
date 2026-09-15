//! Loop state placeholder. Real wrap-around (in/out points, re-anchoring on wrap) is future work;
//! the type exists so `Flow`/`Deck` can carry an optional loop from the start.

/// An active loop between two frames. Not yet enforced during playback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoopState {
    /// Loop start, in frames.
    pub start_frame: u64,
    /// Loop end (exclusive), in frames.
    pub end_frame: u64,
    /// Whether the loop is engaged.
    pub enabled: bool,
}

impl LoopState {
    /// A disabled loop spanning `..total_frames`, i.e. "play to the end".
    pub fn disabled(total_frames: u64) -> Self {
        Self {
            start_frame: 0,
            end_frame: total_frames,
            enabled: false,
        }
    }
}
