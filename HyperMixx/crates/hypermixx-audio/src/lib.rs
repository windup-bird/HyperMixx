//! HyperMixx 音频引擎：输出后端、解码/缓存读取、deck 实时处理、DSP。

pub mod backend;
pub mod deck;
pub mod decode;
pub mod dsp;
pub mod engine;
pub mod fx;
pub mod keylocker;
pub mod track_cache;

pub use fx::{EffectId, EffectManifest, EffectProcessor, FxContext, FxRack};
pub use keylocker::{Keylocker, TimestretchLocker};

pub use backend::{AudioBackend, AudioStream, CpalBackend, DeviceInfo};
pub use engine::{Engine, EngineHandle, EngineOp};
