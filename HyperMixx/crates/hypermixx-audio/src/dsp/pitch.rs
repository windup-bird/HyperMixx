//! PitchShifter：key shift（变调不变速），rubato 5.0 sinc 重采样级。
//!
//! # 与 keylock 引擎的串联数学（勿改动推导结论）
//! 引擎输出 E[n] = x_pc(r_eng·n)：keylock 保住原音高、速率 r_eng。
//! 本级别以每输出帧前进 a 输入帧的方式消费：O[n] = E[a·n] = x_pc(r_eng·a·n)，
//! 音高 ×a、速率 ×(r_eng·a)。令 a = p（p = 2^(半音/12) 目标音高因子）：
//! r_eng = r/p，最终速率 (r/p)·p = r ✓、音高 ×p ✓。
//! rubato 约定 ratio = 输出/输入（ratio>1 = 变慢降调），每输出帧前进
//! 1/ratio → **set_resample_ratio(1/p)**，deck 侧引擎速率 = r/p。
//!
//! # 已知取舍（rubato 文档明示）
//! ratio 动态下调（升调）超过几个百分点时，sinc 抗混叠滤波按构造值
//! （f_cutoff 0.95）生效，高频内容可能混叠。DJ 升调多为瞬时监听，
//! ±12 半音范围内可接受；M3 若需精修，trait 缝在 keylocker（换带目标
//! 音高的引擎方案），本模块不用动。
//!
//! # 零分配
//! 所有缓冲构造时预留：最坏 1/p = 2 → 首块需求 256p + interpolator(513)
//! + ramp 过冲（≈192）≈ 1217 帧，carry 稳态 < 1800 帧，容量 4096 帧
//!   永不触发增长。**调用方每块必须供给 256×p 帧输入**（见 process()）。
//!
//! EOF 时最后 ≈21ms（sinc 尾延迟）被截断——key shift 不在 0 时曲尾略短，
//! 可接受。

use rubato::Adjustable as _;
use rubato::Resampler as _;
use rubato::{Async, FixedAsync};

/// 每块输出帧数（与引擎块一致）。
pub const PITCH_BLOCK_FRAMES: usize = 256;

pub struct PitchShifter {
    rs: Option<Async<f32>>,
    /// 余量缓冲（交织立体声）：上一块未被消费的输入。
    carry: Vec<f32>,
    /// 平面缓冲：输入（预留最坏需求）与输出（固定块长）。
    in_planes: Vec<Vec<f32>>,
    out_planes: Vec<Vec<f32>>,
    pitch_factor: f64,
    bypass: bool,
}

impl PitchShifter {
    /// 输入平面容量（帧）：最坏需求 = 512 + 2×sinc_len + 余量。
    const CARRY_CAP_FRAMES: usize = 4096;

    pub fn new() -> Self {
        let params = rubato::SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: Some(0.95),
            oversampling_factor: 256,
            interpolation: rubato::SincInterpolationType::Linear,
            window: rubato::WindowFunction::BlackmanHarris2,
        };
        // ratio=1.0 起步，max_relative_ratio 2.0 → 运行期 [0.5, 2.0] = ±12 半音。
        // 参数与 To48k 一致（构造失败几乎不可能，仅内存不足；失败 → 永久旁路）。
        let rs = Async::new_sinc(1.0, 2.0, &params, PITCH_BLOCK_FRAMES, 2, FixedAsync::Output).ok();
        Self {
            rs,
            carry: Vec::with_capacity(Self::CARRY_CAP_FRAMES * 2),
            in_planes: vec![
                vec![0.0; Self::CARRY_CAP_FRAMES],
                vec![0.0; Self::CARRY_CAP_FRAMES],
            ],
            out_planes: vec![vec![0.0; PITCH_BLOCK_FRAMES], vec![0.0; PITCH_BLOCK_FRAMES]],
            pitch_factor: 1.0,
            bypass: true,
        }
    }

    /// 设置 key shift（半音，clamp ±12）。0 = 旁路直通（省 CPU）。
    pub fn set_semitones(&mut self, semitones: f64) {
        let semitones = semitones.clamp(-12.0, 12.0);
        self.bypass = semitones.abs() < 1e-6;
        self.pitch_factor = 2f64.powf(semitones / 12.0);
        if let Some(rs) = self.rs.as_mut() {
            // 1/p ∈ [0.5, 2.0] 必在允许范围；ramp=true 半音切换逐块平滑过渡。
            let _ = rs.set_resample_ratio(1.0 / self.pitch_factor, true);
        }
    }

    /// 当前音高因子 p（deck 据此算引擎速率 r/p）。
    pub fn pitch_factor(&self) -> f64 {
        self.pitch_factor
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypass
    }

    /// 处理一块：input 为上游整块输出（交织立体声，帧数由调用方按
    /// 256×p 供给），out 写回恰 frames 帧。旁路时直通拷贝。
    /// 输入不足以凑满一个消费块时输出静音（冷启动/比例突变的正常暂态）。
    ///
    /// 每块消费量 = 256×p（rubato FixedAsync::Output 稳态输入需求，
    /// interpolator 重叠在 last_index 中抵消）——调用方必须按此供给
    /// 引擎渲染帧数，否则 carry 会持续漂移。
    pub fn process(&mut self, input: &[f32], out: &mut [f32], frames: usize) {
        let Some(rs) = self.rs.as_mut() else { return };
        if self.bypass {
            out[..frames * 2].copy_from_slice(&input[..frames * 2]);
            return;
        }
        self.carry.extend_from_slice(input);
        let mut written = 0;
        while written < frames {
            let needed = rs.input_frames_next();
            if self.carry.len() < needed * 2 {
                break;
            }
            // 交织 → 平面（写预留缓冲，零分配）
            for (i, plane) in self.in_planes.iter_mut().enumerate() {
                for (f, p) in plane.iter_mut().enumerate().take(needed) {
                    *p = self.carry[f * 2 + i];
                }
            }
            let Ok(input) = rubato::audioadapter_buffers::direct::SequentialSliceOfVecs::new(
                &self.in_planes,
                2,
                needed,
            ) else {
                break;
            };
            let Ok(mut output) =
                rubato::audioadapter_buffers::direct::SequentialSliceOfVecs::new_mut(
                    &mut self.out_planes,
                    2,
                    PITCH_BLOCK_FRAMES,
                )
            else {
                break;
            };
            let Ok((consumed, produced)) = rs.process_into_buffer(&input, &mut output, None) else {
                break;
            };
            // 平面输出 → 交织写回 out
            for f in 0..produced {
                let o = (written + f) * 2;
                out[o] = self.out_planes[0][f];
                out[o + 1] = self.out_planes[1][f];
            }
            self.carry.drain(..consumed * 2);
            written += produced;
        }
        // 本块凑不满消费块：剩余输出静音（下一块补上）。
        for v in out[written * 2..frames * 2].iter_mut() {
            *v = 0.0;
        }
    }
}

impl Default for PitchShifter {
    fn default() -> Self {
        Self::new()
    }
}
