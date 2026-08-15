//! 引擎：音频回调核心。
//! 每块：应用挂起操作 → 每 deck 快照参数 + 处理 → 混音 → master 软限幅。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::backend::{AudioBackend, AudioStream};
use crate::deck::Deck;
use crate::dsp::smoother::Smoother;
use hypermixx_core::{ControlBus, ControlHandle};

/// 交叉推子增益因子（最简曲线）：居中两边全音量，向一侧移动线性衰减另一侧。
/// x=0 时 (1,1) 精确 → 默认 bitwise 恒等。deck0=A（左）、deck1=B（右）。
pub fn crossfade_factors(x: f32) -> (f32, f32) {
    let x = x.clamp(-1.0, 1.0);
    if x > 0.0 {
        (1.0 - x, 1.0)
    } else {
        (1.0, 1.0 + x)
    }
}

pub enum EngineOp {
    Load { deck: usize, path: PathBuf },
    Seek { deck: usize, seconds: f64 },
    /// 精确跳转（不量化；cue/hotcue 召回用）。
    SeekExact { deck: usize, seconds: f64 },
    /// 按拍跳跃（拍长匹配当前速度，不量化）。
    BeatJump { deck: usize, beats: f64 },
    /// 激活/调整 beat loop（量化起止）。
    SetBeatLoop { deck: usize, beats: f64 },
}

/// UI/MIDI 侧持有的引擎句柄：推送操作，由音频回调在块边界消费。
#[derive(Clone)]
pub struct EngineHandle {
    ops: Arc<Mutex<VecDeque<EngineOp>>>,
}

impl EngineHandle {
    pub fn load(&self, deck: usize, path: impl Into<PathBuf>) {
        self.ops.lock().unwrap().push_back(EngineOp::Load {
            deck,
            path: path.into(),
        });
    }

    pub fn seek(&self, deck: usize, seconds: f64) {
        self.ops
            .lock()
            .unwrap()
            .push_back(EngineOp::Seek { deck, seconds });
    }

    pub fn seek_exact(&self, deck: usize, seconds: f64) {
        self.ops
            .lock()
            .unwrap()
            .push_back(EngineOp::SeekExact { deck, seconds });
    }

    pub fn beatjump(&self, deck: usize, beats: f64) {
        self.ops
            .lock()
            .unwrap()
            .push_back(EngineOp::BeatJump { deck, beats });
    }

    pub fn set_beat_loop(&self, deck: usize, beats: f64) {
        self.ops
            .lock()
            .unwrap()
            .push_back(EngineOp::SetBeatLoop { deck, beats });
    }
}

pub struct Engine {
    pub handle: EngineHandle,
    _stream: Box<dyn AudioStream>,
}

impl Engine {
    pub const SAMPLE_RATE: u32 = 48_000;
    pub const BLOCK_FRAMES: usize = 256;
    pub const DECKS: usize = 2;

    pub fn start(
        backend: &dyn AudioBackend,
        bus: &ControlBus,
        device: Option<&str>,
    ) -> Result<Engine> {
        let (mut state, handle) = Self::core(bus);
        let stream = backend.open(
            device,
            Self::SAMPLE_RATE,
            Self::BLOCK_FRAMES,
            Box::new(move |out| state.process(out)),
        )?;
        stream.play()?;
        Ok(Engine {
            handle,
            _stream: stream,
        })
    }

    /// 无设备构造引擎核心（测试/基准用）：外部用定时器驱动 process() 模拟音频回调。
    pub fn core(bus: &ControlBus) -> (EngineState, EngineHandle) {
        let ops = Arc::new(Mutex::new(VecDeque::new()));
        let coeff = 1.0 - (-1.0 / (0.01 * Self::SAMPLE_RATE as f64)).exp();
        let state = EngineState {
            decks: [
                Deck::new(0, Self::SAMPLE_RATE, bus),
                Deck::new(1, Self::SAMPLE_RATE, bus),
            ],
            ops: ops.clone(),
            tmp: vec![0.0; Self::BLOCK_FRAMES * 2],
            master_volume: bus.control(hypermixx_core::paths::master_volume()),
            master_vu: bus.control(hypermixx_core::paths::master_vu()),
            crossfader: bus.control(hypermixx_core::paths::master_crossfader()),
            // 初始 1.0：交叉推子居中时因子恒 1.0（bitwise 恒等）
            xfa: Smoother::new(1.0, coeff as f32),
            xfb: Smoother::new(1.0, coeff as f32),
        };
        (state, EngineHandle { ops })
    }
}

pub struct EngineState {
    decks: [Deck; 2],
    ops: Arc<Mutex<VecDeque<EngineOp>>>,
    tmp: Vec<f32>,
    master_volume: ControlHandle,
    master_vu: ControlHandle,
    crossfader: ControlHandle,
    /// 交叉推子因子逐采样平滑（10ms，防拖动拉链声）。
    xfa: Smoother,
    xfb: Smoother,
}

impl EngineState {
    pub fn process(&mut self, out: &mut [f32]) {
        // 1. 应用挂起操作（try_lock：不与 UI 线程争抢）
        if let Ok(mut q) = self.ops.try_lock() {
            while let Some(op) = q.pop_front() {
                match op {
                    EngineOp::Load { deck, path } => {
                        if deck < self.decks.len() {
                            log::info!("加载 deck {}: {}", deck + 1, path.display());
                            self.decks[deck].load(path);
                        }
                    }
                    EngineOp::Seek { deck, seconds } => {
                        if deck < self.decks.len() {
                            self.decks[deck].seek_seconds(seconds);
                        }
                    }
                    EngineOp::SeekExact { deck, seconds } => {
                        if deck < self.decks.len() {
                            self.decks[deck].seek_exact(seconds);
                        }
                    }
                    EngineOp::BeatJump { deck, beats } => {
                        if deck < self.decks.len() {
                            // P14：仅跳目标轨（P12 联动跳已删——用户反馈
                            // sync 后两轨一起跳不符合预期）。简单跳拍：
                            // 各自独立落 grid 拍；sync 速率锁保持 BPM 一致。
                            self.decks[deck].beatjump(beats);
                        }
                    }
                    EngineOp::SetBeatLoop { deck, beats } => {
                        if deck < self.decks.len() {
                            self.decks[deck].set_beat_loop(beats);
                        }
                    }
                }
            }
        }

        // 2. 每 deck 快照参数 → beat sync → 处理 + 混音
        out.fill(0.0);
        let frames = out.len() / 2;
        for deck in self.decks.iter_mut() {
            deck.update_params();
        }
        // beat sync：sync 开启的 deck 跟随另一 deck（双开时 deck1 随 deck0）
        let (fi, li) = match (self.decks[0].sync_on(), self.decks[1].sync_on()) {
            (true, true) | (false, true) => (1, 0),
            (true, false) => (0, 1),
            (false, false) => (usize::MAX, usize::MAX),
        };
        if fi < self.decks.len() {
            let leader = self.decks[li].sync_leader_snapshot();
            self.decks[fi].apply_sync(&leader);
        }
        // 交叉推子：居中两边全音量，向一侧线性衰减另一侧（因子逐采样平滑）
        let (fa, fb) = crossfade_factors(self.crossfader.get() as f32);
        self.xfa.set_target(fa);
        self.xfb.set_target(fb);
        for (i, deck) in self.decks.iter_mut().enumerate() {
            deck.process(&mut self.tmp, frames);
            let factor = if i == 0 { &mut self.xfa } else { &mut self.xfb };
            for (o, t) in out.iter_mut().zip(self.tmp.iter()) {
                *o += *t * factor.step();
            }
        }

        // 3. master：音量 + 软限幅 + 防 denormal
        let mv = self.master_volume.get().clamp(0.0, 1.0) as f32;
        for v in out.iter_mut() {
            *v = (*v * mv).tanh() * 0.95;
        }
        crate::dsp::sanitize(out);
        let mut peak = 0.0f32;
        for v in out.iter() {
            let a = v.abs();
            if a > peak {
                peak = a;
            }
        }
        self.master_vu.set(peak as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::{crossfade_factors, Engine, EngineHandle, EngineState};
    use crate::deck::{
        test_deck_with_ring_and_prod, test_refill_after_seek, test_sine_chunks,
    };
    use hypermixx_core::paths;

    /// 双 deck 测试台：deck0（grid 120，bus_a）+ deck1（grid 124，bus_b）。
    /// 独立 bus 使两轨可设不同 grid；deck1 的 sync 经 bus_b 控制。
    /// 返回 prod：测试 deck 无 reader 线程，beatjump seek 清空 ring 后
    /// 须补推新世代 chunk 模拟 reader（否则欠载、播头冻结）。
    fn dual_deck_rig() -> (
        EngineState,
        EngineHandle,
        hypermixx_core::ControlBus,
        hypermixx_core::ControlBus,
        ringbuf::HeapProd<crate::caching_reader::Chunk>,
        ringbuf::HeapProd<crate::caching_reader::Chunk>,
    ) {
        let bus_a = hypermixx_core::ControlBus::default();
        let bus_b = hypermixx_core::ControlBus::default();
        let (mut state, handle) = Engine::core(&bus_a);
        let (deck0, prod0) = test_deck_with_ring_and_prod(&bus_a, test_sine_chunks(64), 0.0);
        let (deck1, prod1) = test_deck_with_ring_and_prod(&bus_b, test_sine_chunks(64), 0.0);
        state.decks[0] = deck0;
        state.decks[1] = deck1;
        bus_a.control(&paths::deck_grid_bpm(0)).set(120.0);
        bus_a.control(&paths::deck_grid_offset(0)).set(0.0);
        bus_b.control(&paths::deck_grid_bpm(0)).set(124.0);
        bus_b.control(&paths::deck_grid_offset(0)).set(0.0);
        (state, handle, bus_a, bus_b, prod0, prod1)
    }

    /// 跑 blocks 块（256 帧/块）。
    fn run_blocks(state: &mut EngineState, blocks: usize) {
        let mut out = vec![0.0f32; Engine::BLOCK_FRAMES * 2];
        for _ in 0..blocks {
            state.process(&mut out);
        }
    }

    #[test]
    fn crossfader_center_is_unity() {
        let (a, b) = crossfade_factors(0.0);
        assert_eq!(a, 1.0);
        assert_eq!(b, 1.0);
    }

    #[test]
    fn crossfader_extremes_kill_opposite_side() {
        let (a, b) = crossfade_factors(1.0);
        assert_eq!(a, 0.0);
        assert_eq!(b, 1.0);
        let (a, b) = crossfade_factors(-1.0);
        assert_eq!(a, 1.0);
        assert_eq!(b, 0.0);
    }

    #[test]
    fn crossfader_midpoints_are_linear() {
        let (a, b) = crossfade_factors(0.5);
        assert!((a - 0.5).abs() < 1e-6, "a={a}");
        assert_eq!(b, 1.0);
        let (a, b) = crossfade_factors(-0.25);
        assert_eq!(a, 1.0);
        assert!((b - 0.75).abs() < 1e-6, "b={b}");
    }

    #[test]
    fn crossfader_clamps_out_of_range() {
        let (a, b) = crossfade_factors(2.0);
        assert_eq!(a, 0.0);
        assert_eq!(b, 1.0);
        let (a, b) = crossfade_factors(-2.0);
        assert_eq!(a, 1.0);
        assert_eq!(b, 0.0);
    }

    /// P14：sync 开启时 beatjump 不联动——仅目标轨跳拍（P12 联动已删，
    /// 用户反馈 sync 后两轨一起跳不符合预期），另一轨按 sync 速率锁
    /// 继续推进（grid 124 锁 leader 120 BPM → 音轨速率 120/124）。
    #[test]
    fn beatjump_with_sync_jumps_only_target_deck() {
        let (mut state, handle, _bus_a, bus_b, mut prod0, _prod1) = dual_deck_rig();
        bus_b.control(&paths::deck_sync(0)).set(1.0); // deck1 sync 跟随
        run_blocks(&mut state, 240); // 预热 1.28s：一次性对齐完成、速率锁

        let p0 = state.decks[0].ctl.playhead.get();
        let p1 = state.decks[1].ctl.playhead.get();
        assert!(p0 > 0.2 && p1 > 0.2, "预热后应在播：p0={p0} p1={p1}");

        // P16（P17 回滚）：跳距精确 4 拍（不吸附网格）
        let want0 = p0 + 4.0 * 60.0 / 120.0;

        handle.beatjump(0, 4.0); // 跳 deck0（leader）→ 不联动 deck1
        run_blocks(&mut state, 1); // 应用 op：deck0 seek、ring 清空
        // 补喂（模拟 reader 响应 Seek）：不补则欠载、播头冻结在 feed_base
        test_refill_after_seek(&mut state.decks[0], &mut prod0, want0);
        run_blocks(&mut state, 8); // warm_start priming + 收敛

        let q0 = state.decks[0].ctl.playhead.get();
        // P14 最小预卷（无 priming 丢弃偏移）：播头 = 落点 − 560 帧引擎
        // 延迟补偿 + 推进（与常规 seek 稳态语义一致）
        let expect0 = want0 - 560.0 / 48000.0 + 8.0 * 256.0 / 48000.0;
        assert!((q0 - expect0).abs() < 0.005, "deck0 未跳到落点：{q0} vs {expect0}");

        // deck1 不联动：9 块（跳后共 1+8）按 sync 速率锁继续推进
        //（grid 124 锁 leader 120 BPM → 音轨速率 120/124）
        let q1 = state.decks[1].ctl.playhead.get();
        let adv1 = 9.0 * 256.0 / 48000.0 * (120.0 / 124.0);
        assert!(
            (q1 - p1 - adv1).abs() < 0.01,
            "deck1 不应被联动：{q1} vs {p1}（期望推进 {adv1:.4}s）"
        );
    }

    /// P12：sync 全关时不联动——只跳目标轨，另一轨继续正常推进。
    #[test]
    fn beatjump_without_sync_jumps_only_target() {
        let (mut state, handle, _, _, mut prod0, _prod1) = dual_deck_rig();
        run_blocks(&mut state, 60);

        let p0 = state.decks[0].ctl.playhead.get();
        let p1 = state.decks[1].ctl.playhead.get();
        // P16（P17 回滚）：跳距精确 4 拍（不吸附网格）
        let want0 = p0 + 4.0 * 60.0 / 120.0;

        handle.beatjump(0, 4.0);
        run_blocks(&mut state, 1); // 应用 op：deck0 seek、ring 清空
        test_refill_after_seek(&mut state.decks[0], &mut prod0, want0);
        run_blocks(&mut state, 8); // priming + 收敛
        let q0 = state.decks[0].ctl.playhead.get();
        // P14 最小预卷：播头 = 落点 − 560 帧延迟补偿 + 推进
        let expect0 = want0 - 560.0 / 48000.0 + 8.0 * 256.0 / 48000.0;
        assert!((q0 - expect0).abs() < 0.005, "deck0 未跳到落点：{q0} vs {expect0}");

        // deck1 无 sync 不联动：9 块（跳后共 1+8）正常推进
        let q1 = state.decks[1].ctl.playhead.get();
        assert!(
            (q1 - p1 - 9.0 * 256.0 / 48000.0).abs() < 0.01,
            "deck1 不应被联动：{q1} vs {p1}"
        );
    }

    /// P12：leader 停播后 follower 的 sync 速率保持（bpm 连续），不再
    /// 瞬跳回滑杆值（跳变会经 effBpm 使拍轴窗口平移——"到结束时右移"）。
    /// 后半验证无网格 fallback 下滑杆仍可调速（回归防护）。
    #[test]
    fn sync_follower_rate_holds_after_leader_stops() {
        let (mut state, _handle, bus_a, bus_b, _prod0, _prod1) = dual_deck_rig();
        bus_b.control(&paths::deck_sync(0)).set(1.0);
        run_blocks(&mut state, 120); // 预热 + sync 收敛

        // follower grid 124 追 leader 120 → rate ≈ 0.9677 → bpm ≈ 120
        let bpm_synced = state.decks[1].ctl.bpm.get();
        assert!(
            (bpm_synced - 120.0).abs() < 1.0,
            "sync 后 follower bpm 应 ≈ leader 120：{bpm_synced}"
        );

        // leader 停播 → follower apply_sync 提前返回；P12 修复后 rate 保持
        bus_a.control(&paths::deck_play(0)).set(0.0);
        run_blocks(&mut state, 2);
        let bpm_after = state.decks[1].ctl.bpm.get();
        assert!(
            (bpm_after - bpm_synced).abs() < 0.01,
            "leader 停后 follower bpm 应连续（不跳回 124×滑杆）：{bpm_after} vs {bpm_synced}"
        );
        let p1a = state.decks[1].ctl.playhead.get();
        run_blocks(&mut state, 60);
        let p1b = state.decks[1].ctl.playhead.get();
        assert!(p1b > p1a + 0.1, "follower 应继续播放：{p1a} → {p1b}");

        // 无网格 fallback：grid 清 0 → sync 失效回滑杆，滑杆仍可调速
        bus_b.control(&paths::deck_grid_bpm(0)).set(0.0);
        bus_b.control(&paths::deck_rate(0)).set(4.0);
        run_blocks(&mut state, 200);
        let adv = state.decks[1].ctl.playhead.get() - p1b;
        assert!(
            (adv - 200.0 * 256.0 / 48000.0 * 1.04).abs() < 0.03,
            "无网格 fallback 滑杆 +4% 应生效：adv={adv}"
        );
    }
}
