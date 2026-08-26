# global-rigid-grid

**date**
8.22

**done**
- 后分析刚性网格
- 波形预览，y轴zscore

**fuck**
- 3-band可视性，颜色和频段
- rgb颜色
- 竖条太粗
- flutter滚动波形卡顿，边缘闪现，

# hyper-sync

**date**
8.23

**done**
- 三段式sync，点一次对齐网格和速度，点两次pitchlock，点三次取消
- loop出入bug
- beatjump交叉
- 基于网格的beatjump落点

**fuck**
- sync对不准
- beatjump偏移
- beatjump后变速

# hypermixx-ts014

**date**
8.26

**done**
- timestretch 0.11 → 0.14：低频段音高校正、downbeat 低频能量选举
  （修 snare 落重拍）、DnB 度量表；KEYLOCK 延迟 560→610 帧
- seek/cue/beatjump 统一官方 desktop 参考协议：`reset → set_track_position(target−preroll)
  → warm_start(preroll) → 立即补推预热区`，全质量预热，接受 ~30ms
  操作到输出延迟换时间轴严格（用户取舍：可接受延迟，不可接受轨间相位误差）
- 删除接缝填充/掩蔽机制全套（capture_jump_filler/apply_beatjump_blend/
  beatjump_snap/mask）——被官方协议取代
- 环容量 8192→32768 帧（对齐官方参考，支撑大占用调度余量）
- analysis clippy 存量告警清零（as_chunks）
- 测试重构：seam 有界静音窗断言、时移诊断（实测 ~29ms=预热跑图期，
  上界 0.04 防异常）、sync follower 容差随 KEYLOCK_LATENCY 动态化

**fuck**
- beatjump 后对 leader 相位仍滞后 ≈ 预热跑图期（~30ms）——官方协议
  固有；后续可选「同回调爆发排干」归零（设计已验证可行）

# hyper-sync 重构：sync 曲线重设计 + beatjump 接缝修复

**date**
8.26

**done**
- **sync 曲线重写**（deck.rs `apply_sync`）：tempo 开启沿瞬锁目标速率；
  相位线性平移——远区恒定 ±8% 追相位，**误差过零即锁**（残差 ≤ 单步
  ≈0.4ms@120BPM）。旧 smoothstep+0.01 拍死区随机停住遗留 ≤10ms 永久
  稳态偏差，已消除。两个陷阱记录：单块精确闭合被引擎 set_rate 平滑
  打破（振荡不收敛）；窄锁定窗会被整步飞越错过（绕圈）——过零判定
  对任意相对运动确定性终止。
- **stage-2 盲取消 bug**：快速连点 sync（1→2）曾无条件置
  `sync_align_done=true` 中止对齐且永不重试；现保留 pending 至收敛。
- **beatjump 与 sync 完全解耦**：删除 sync 下跳拍的重新对齐触发
  （旧路径置 pending → 相位修正弯折速率 =「jump 改变播放速率」根因，
  最高 ±100%×eased 持续约秒级）。整数拍按 grid 坐标推进天然保相。
- **beatjump 接缝填充**（借鉴 Mixxx readToCrossfadeBuffer 思想）：
  - 引擎侧根因：timestretch 前瞻管线 reset 后需重填充（~560 帧），
    该声学空洞在数学上不可消除（前瞻链需要 L 帧未来上下文）
  - 方案 = 加性填充：从出声位置直读缓存原始采样续读（旧音频的真正
    延续），包络 smoothstep 渐入渐出；前 ~64 帧由引擎自带 release
    ramp 承载（与已听内容波形连续）。两轮失败教训：回放历史窗口
    （时间倒退阶跃）、直接拼接原始采样（keylock 颗粒重组改变波形，
    引擎输出≠原始采样）
  - 配套：min_preroll 1帧→pipeline_latency_frames()，stage 链在真实
    内容上收敛，消除重填充边界新内容单样本硬进入（Δ 0.37→0.04）
- **跨平台自适应**：所有尺寸构建期查询 keylocker，零硬编码帧数——
  填充容量 next_pow2(latency)、掩蔽长度 = latency、测试容差动态推导。
- 测试：`beatjump_seam_no_silence_gap`（±30ms 无静音洞）、
  `beatjump_seam_blend_no_click`（逐采样 Δ<0.05）、
  `beatjump_integer_phase_lag_diagnostic`（时移量化基线）、
  `sync_follower_own_jump_no_realign` 容差随 mask_len 动态化。
- 全量 cargo test --workspace 通过（165+20），clippy 零警告。

**调研**
- Mixxx 参考结论（~/Git/mixxx）：同步拉取渲染 vs 本项目推喂管线——
  Mixxx 无引擎内延迟、seek 同回调交叉淡化零空洞；loop 在读路径内联
  换向；sync 用连续 P 控制器吸收一切瞬态。本项目 P14 刻意删连续修正
  （用户微调不被拉回），故每个转换须自身精确。
- 预热预算论证：timestretch prime budget ≤512 帧/回调 ≈ 2×实时，
  提预热帧数不缩短静音窗（与管线填充同速），仅换质量。

**fuck**
- sync 下跳后残余时移 = 管线延迟量级（实测 ~17ms@48k）：掩蔽消静音洞
  不消时移，对 leader 有轻微 flam——待影子引擎交接里程碑消除
- timestretch 为 crates.io 依赖，位置调度（无 reset 重锚）需 vendor 补丁

# plan-26：记拍器 + 持续相位锁定（M1）

**date**
8.26

**done**
- **BarClock**（core/beatgrid.rs）：小节时钟 `bar_index / beat_in_bar
  / beat_phase`，4/4 参数化，小节原点对齐 analysis downbeat_rotation
  （乐句真起点）；`from_grid_at(_bpb)` 纯函数可测。
- **downbeat_rotation 贯穿**：bridge 发布 TrackAnalysis 时由 `downbeats_secs[0]
  - offset` 推导旋转写入 `deck_grid_rotation`（无 downbeat 退化 0）；
  载曲清零一并复位。deck 每块快照 `bar_rotation` 用于 BarClock。
- **持续相位锁定 P26**（deck.rs `apply_sync`）：一次性对齐后进入稳态，
  按 `err`（拍）比例修正 `corr = clamp(0.05×err, ±0.5%)`，**仅作用引擎轴**
  （`engine_rate() × (1+corr)`），不动 `self.rate`——BPM 显示 / FX 拍时钟
  零抖动。三处挂起：nudge 激活（±8% 抢修正）、|err|≥0.1 拍（用户主动
  偏移 P14 保留）、死区 <5e-4 拍（防 limit-cycle）；seek/load 复位。
- 测试：BarClock 数学（4/4、rotation 平移、负小节、3/4 退化）、
  `sync_continuous_corr_proportional_bounded_nudge_gated`（比例/钳幅/
  引擎轴/self.rate 隔离/nudge 挂起，逻辑级）、
  `sync_continuous_corr_stays_bounded_over_long_run`（10s 不发散不漂移）。
- 全量 210 测试通过，clippy 零警告。

**note**
- 持续修正只校正「零活小漂移带」——与 P14「用户 seek/jump 相位差保留」
  通过上界 0.1 拍调和：带内漂移被无声拉回，离散大偏移不再强拉。
- set_rate_at（采样精确时间戳调度）尚未启用：稳态修正 ≤0.5% 经 ASAP
  set_rate 于 32 帧重采样块平滑已不可闻；时间戳调度留作将来一次性对齐
  线性段及更长曲线。

# plan-26：beatjump 落点无缝（M2）

**date**
8.26

**done**
- **同回调爆发排干**（deck.rs）：seek 时若缓存可喂满全预卷（`fed ==
  preroll`）→ 置 `burst_preroll`；process_engine 内按 `ceil(preroll /
  clamp(2×engine_frames, 256, 2048))` 烧 dummy process 排干 warm_start
  priming（调用次数与引擎 `PRIME_BUDGET_*` 一致，版本锚定测试保护）。
  真实 process 同块即产出目标内容——**接缝静音窗 30ms → 0ms**。
- **旧尾交叉淡化 128 帧**（`JUMP_CROSSFADE_FRAMES`）：跳转块前 128 帧混合
  旧内容（`read_cache_stereo` 缓存直读续播，`jump_old_base`=跳前出声位置）
  × cos 淡出 + 引擎新内容 × sin 淡入（等功率）。整数拍跳距新旧相位恒等
  → 混合拍对齐，无 click。自由函数 `read_cache_stereo` 复用缓存读，
  与 `self.engine_scratch` 可变借用并存（避免 `read_stereo` 的 &mut self
  借用冲突）。
- **回退保留**：缓存欠载（`fed < preroll`）→ `burst_preroll=0` → 官方协议
  静音窗（未变），不引入新路径风险。
- **修复环内跳拍边角 bug**：seek 重建环相位改用 `feed_pos`（续喂点）判别
  环内，原 `read_frame >= li` 在 target 距环头 < preroll 时误清环。
- 测试：`beatjump_seam_gap_bounded` 收紧静音窗 <6ms + 接缝 ±16 帧无 click
  （saw 整曲每 24000 帧天然回绕故只测紧贴接缝窗）；
  `beatjump_burst_engages_crossfade_and_lands_exact`（burst 置位/耗尽、
  交叉置位/耗尽、跳距 4 拍精确、播头推进无回退）。
- 全量 211 测试通过，clippy 零警告。

**note**
- 旧尾交叉在引擎输出原域采样机含量（keylock 关 + 变速时新旧 pitch 微差，
  128 帧≈2.7ms 内不可闻）；默认 keylock 开时恒等，无影响。
- 环内跳拍的爆发 feed 按线性 cache 读（短环 + 目标近环头时可能跨环界读
  越界内容），与实际环折叠喂入在极短环下有偏差——低频次边角，留待
  Wide profile / 极短环特例统一。
- Wide profile（preroll 4096 > 预算硬顶 2048）爆发下限 2 块：仍有 ≤1 块
  静音窗（10.6ms），远小于旧 30ms，接受。

# plan-26：pre_analysis 工件接入（M3）

**date**
8.26

**done**
- **引擎构建期注入工件**（keylocker.rs）：`build_with_analysis(sr, wide,
  Option<Arc<PreAnalysisArtifact>>)`，`EngineConfig.pre_analysis` 由它接管
  （timestretch 无运行时注入 API，只能构建期带）。`build` 委派 None。
- **工件构造**（deck.rs）：`PreAnalysisData`（bpm/offset/beats_secs/
  downbeats_secs/confidence/tempo_segments，桥接层向引擎薄传）+ 
  `build_pre_analysis_artifact` → `PreAnalysisArtifact`（48k 绝对帧
  beat_positions、downbeat_beat_indices、TempoSegment；transient_onsets 留空
  → 引擎退化为「按拍/downbeat 对齐拼接，无 onset 知识」，恰是本增产力点）。
- **异步-构建期时序调和**（用户选「分析完成后安全重建」）：桥接层
  `forward_events` 接到高置信 TrackAnalysis → `EngineHandle::set_pre_analysis`
  推 op；引擎回调 `SetPreAnalysis` → `deck.set_pre_analysis`：存储工件，
  **非播放时立即重建引擎**（播放中不打扰，只存；profile 切换/下次 rebuild
  自动带上）。deck `load`/`rebuild_keylocker` 均用 `pre_analysis.clone()` 构建。
- 测试：`build_pre_analysis_maps_beatgrid`（拍点/downbeat/分段映射到 48k
  帧、downbeat_offset_samples、无拍 None）；桥接 forwarder 测试适配新签名。
- 全量 212 测试通过，clippy 零警告。

**note**
- 工件仅提升 keylock 拼接质量（SOLA 避开拍/乐句边界），非正确性；初次
  build 常无工件（分析异步）——播放中载入曲目的首段 keylock 走通用拼接，
  分析完成且暂停后重建才带。可接受（用户决策）。
- 版本耦合：`PREANALYSIS_VERSION`(=13) 随 timestretch 升级需核对
  MIN_COMPATIBLE；锚在 `build_pre_analysis_artifact`。
- 播放中不自动重建避免了 mid-mix 可闻 blip（~45ms 管线填充），换覆盖率。

