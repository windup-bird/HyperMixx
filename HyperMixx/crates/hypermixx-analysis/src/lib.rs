//! 曲目分析：RGB 波形、BPM、调性检测（M2 加入后两者）。

pub mod energy;
pub mod grid_fit;
pub mod mono;
pub mod segment;
pub mod waveform;

pub use segment::{AnalysisEvent, SEG_COLS, SEG_FRAMES, start_analysis};
pub use timestretch::core::preanalysis::{KeyEstimate, KeyMode};
pub use waveform::{Column, WaveformData, analyze};
