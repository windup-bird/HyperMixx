//! HyperMixx 核心：控制总线与共享类型。
//! 无 IO、无实时依赖，全项目的最小公共层。

pub mod beatgrid;
pub mod control;

pub use beatgrid::{BeatClock, BeatGrid};
pub use control::{ControlBus, ControlHandle};

use std::path::PathBuf;

/// UI/MIDI → 引擎的操作命令（数值类参数走 ControlBus，命令类操作走这里）。
#[derive(Debug)]
pub enum EngineCommand {
    /// 加载音轨到指定 deck（自动开始播放）。
    Load { deck: usize, path: PathBuf },
    /// 跳转到指定位置（秒，按 48kHz 引擎时间轴）。
    Seek { deck: usize, seconds: f64 },
}

/// 控制点路径常量，避免 UI/引擎之间写魔法字符串。
pub mod paths {
    pub fn deck_play(deck: usize) -> String {
        format!("Deck{}.play", deck + 1)
    }
    pub fn deck_rate(deck: usize) -> String {
        format!("Deck{}.rate", deck + 1)
    }
    pub fn deck_eq_low(deck: usize) -> String {
        format!("Deck{}.eq_low", deck + 1)
    }
    pub fn deck_eq_mid(deck: usize) -> String {
        format!("Deck{}.eq_mid", deck + 1)
    }
    pub fn deck_eq_high(deck: usize) -> String {
        format!("Deck{}.eq_high", deck + 1)
    }
    pub fn deck_volume(deck: usize) -> String {
        format!("Deck{}.volume", deck + 1)
    }
    /// 通道增益（dB，-12..+12，默认 0 = 0dB = ×1.0）。
    pub fn deck_gain(deck: usize) -> String {
        format!("Deck{}.gain", deck + 1)
    }
    /// deck 滤波器旋钮（-1..+1，0 = 旁路；正=低通 20kHz→20Hz，负=高通 20Hz→20kHz）。
    pub fn deck_filter(deck: usize) -> String {
        format!("Deck{}.filter", deck + 1)
    }
    pub fn deck_playhead(deck: usize) -> String {
        format!("Deck{}.playhead", deck + 1)
    }
    pub fn deck_vu(deck: usize) -> String {
        format!("Deck{}.vu", deck + 1)
    }
    pub fn deck_duration(deck: usize) -> String {
        format!("Deck{}.duration", deck + 1)
    }
    pub fn deck_loaded(deck: usize) -> String {
        format!("Deck{}.loaded", deck + 1)
    }
    /// Key shift 半音（-12..12，仅 keylock 开启时生效）。
    pub fn deck_pitch(deck: usize) -> String {
        format!("Deck{}.pitch", deck + 1)
    }
    /// keylock 开关（0/1，默认 1）。
    pub fn deck_keylock(deck: usize) -> String {
        format!("Deck{}.keylock", deck + 1)
    }
    /// 实时 BPM 显示（引擎写 UI 读；grid_bpm × 实际速率）。
    pub fn deck_bpm(deck: usize) -> String {
        format!("Deck{}.bpm", deck + 1)
    }
    /// beatgrid BPM（UI/分析写引擎读；0 = 无网格）。
    pub fn deck_grid_bpm(deck: usize) -> String {
        format!("Deck{}.grid_bpm", deck + 1)
    }
    /// beatgrid 偏移（秒）。
    pub fn deck_grid_offset(deck: usize) -> String {
        format!("Deck{}.grid_offset", deck + 1)
    }
    /// beat sync 开关（0/1，P5）。
    pub fn deck_sync(deck: usize) -> String {
        format!("Deck{}.sync", deck + 1)
    }
    /// quantize seek 开关（0/1，P5）。
    pub fn deck_quantize(deck: usize) -> String {
        format!("Deck{}.quantize", deck + 1)
    }
    /// 对拍临时加减速（-1/0/+1，按钮按住期间生效；sync 开启时被同步覆盖）。
    pub fn deck_nudge(deck: usize) -> String {
        format!("Deck{}.nudge", deck + 1)
    }
    /// beat loop 开关（0/1；UI 写引擎读，外部跳转出环时引擎清零）。
    pub fn deck_loop_active(deck: usize) -> String {
        format!("Deck{}.loop_active", deck + 1)
    }
    /// beat loop 起点（秒，激活时拍网格量化）。
    pub fn deck_loop_in(deck: usize) -> String {
        format!("Deck{}.loop_in", deck + 1)
    }
    /// beat loop 终点（秒，按拍长计算，钳制到曲尾）。
    pub fn deck_loop_out(deck: usize) -> String {
        format!("Deck{}.loop_out", deck + 1)
    }
    /// 缓存填充进度（0..1；引擎写 UI 读，未加载/未知总长时为 0）。
    pub fn deck_cache_filled(deck: usize) -> String {
        format!("Deck{}.cache_filled", deck + 1)
    }
    /// FX 槽位类型（0=空，1..=8 对应 EffectId）。
    pub fn deck_fx_type(deck: usize, slot: usize) -> String {
        format!("Deck{}.fx{}_type", deck + 1, slot + 1)
    }
    /// FX 槽位开关（0/1）。
    pub fn deck_fx_enable(deck: usize, slot: usize) -> String {
        format!("Deck{}.fx{}_enable", deck + 1, slot + 1)
    }
    /// FX 槽位干湿比（0..1；rack 统一混音）。
    pub fn deck_fx_drywet(deck: usize, slot: usize) -> String {
        format!("Deck{}.fx{}_drywet", deck + 1, slot + 1)
    }
    /// FX 槽位参数（自然单位，按位对应 manifest params）。
    pub fn deck_fx_p(deck: usize, slot: usize, p: usize) -> String {
        format!("Deck{}.fx{}_p{}", deck + 1, slot + 1, p + 1)
    }
    pub fn master_volume() -> &'static str {
        "Master.volume"
    }
    pub fn master_vu() -> &'static str {
        "Master.vu"
    }
    /// 交叉推子（-1..+1，0 = 居中两边全音量；向一侧移动线性衰减另一侧）。
    pub fn master_crossfader() -> &'static str {
        "Master.crossfader"
    }
}

#[cfg(test)]
mod tests {
    use super::paths;

    #[test]
    fn deck_paths_are_stable() {
        assert_eq!(paths::deck_gain(0), "Deck1.gain");
        assert_eq!(paths::deck_filter(0), "Deck1.filter");
        assert_eq!(paths::deck_gain(1), "Deck2.gain");
        assert_eq!(paths::deck_filter(1), "Deck2.filter");
    }

    #[test]
    fn master_paths_are_stable() {
        assert_eq!(paths::master_crossfader(), "Master.crossfader");
        assert_eq!(paths::master_volume(), "Master.volume");
        assert_eq!(paths::master_vu(), "Master.vu");
    }
}
