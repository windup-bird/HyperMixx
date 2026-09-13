# Hypermixx 架构

Rust workspace,单二进制 + 库。CLI 发命令到 producer 线程,producer 驱动双 Deck 经 timestretch 引擎混音进 cpal。

```
hypermixx/
├── Cargo.toml              # workspace,profiles
├── scripts/phase_probe.sh  # 双 deck beatjump 相位差探针
│
├── crates/
│   ├── hypermixx-audio/    # 引擎核心(lib)
│   │   ├── src/
│   │   │   ├── lib.rs              # 常量 + re-export
│   │   │   ├── command.rs          # Command / CommandResponse / DeckState 协议
│   │   │   ├── beatgrid.rs         # BeatGrid / TrackAnalysis / KeyReport / KeyMode
│   │   │   ├── ringbuf.rs          # rtrb 封装 + 自由函数
│   │   │   ├── pipeline.rs         # AudioPipeline:producer 线程 + cpal 输出
│   │   │   ├── deck/
│   │   │   │   ├── deck.rs         # Deck:流状态机 + 非阻塞跳转
│   │   │   │   └── timeshift.rs    # TimeShift:后台预热线程
│   │   │   ├── flow/
│   │   │   │   ├── flow.rs         # Flow:单次播放单元(状态机)
│   │   │   │   └── pitchshift.rs   # PitchShiftEngine:timestretch 引擎包装
│   │   │   └── source/
│   │   │       ├── mod.rs          # Source trait
│   │   │       ├── decoder.rs      # symphonia 解码(重采样 / 声道合并)
│   │   │       └── pool.rs         # PcmPool:内存池 Source 实现
│   │   └── tests/                  # 集成测试(真实 wav/mp3)
│   │
│   ├── hypermixx-analysis/ # 节拍/调性分析适配层
│   │   └── src/lib.rs      # stratum-dsp → TrackAnalysis
│   │
│   ├── hypermixx-cli/      # 命令行前端(二进制)
│   │   └── src/main.rs     # 命令解析 + 分析编排 + 输出格式化
│   │
│   ├── stratum-dsp/        # vendored:symphonia 解码后的 DSP 分析
│   └── timestretch/        # vendored:实时时间拉伸引擎
```

---

## hypermixx-audio

### `lib.rs`
引擎常量和公开类型。

| 名称 | 作用 |
|---|---|
| `SAMPLE_RATE` / `CHANNELS` / `BLOCK_SIZE` | 48 kHz / 立体声 / 256 帧 |
| `DECK_COUNT` / `DECK_MIX_GAIN` | 双 Deck,每路 ×0.5 混音 |

### `command.rs`
CLI 与 producer 之间的请求-响应协议。

**`Command`** — 发给 producer 的命令:
- `Load { deck_id, path, bpm }` — 解码;bpm 给定则立即建常网格并跳过分析
- `Play` / `Pause` / `Jump { target_frame }` / `BeatJump { beats }` — 传输控制
- `SetRate { rate }` / `SetProfile { profile }` — 实时拉伸参数
- `SetAnalysis { analysis }` — 异步分析结果回传
- `GetState` / `GetAllStates` / `Quit`

**`CommandResponse`** — producer 回复:
- `Loaded { deck_id, total_frames, analyzed }` — analyzed=true 表示已有网格(CLI 应跳过分析)
- `State(DeckState)` / `States(Vec<DeckState>)` / `Ok` / `Error`

**`DeckState`** — `{ deck_id, current_frame, playing, total_frames, bpm, key }`

### `beatgrid.rs`
音乐节拍网格与调性。

**`TrackAnalysis`** — `{ beatgrid: BeatGrid, key: Option<KeyReport>, bpm: Option<f32> }`

**`BeatGrid`** — 绝对帧位置 `Vec<u64>` (严格递增)。
- `from_seconds(beats_sec, sample)` — 秒制→帧(外部分析器用)
- `from_constant_bpm(bpm, first, total, sample)` — 恒速网格(无累积误差)
- `frame_at_beat(beat)` — 超界按末拍间隔外推
- `nearest_beat` / `current_beat_frame` / `next_beat_frame` / `phase` — 二分查找
- `beatjump_target(frame, beats)` — **相位保持跳转**:最近拍 + 拍内偏移 + 目标拍宽

**`KeyReport`** — `{ pc: u8, mode, confidence }`,`name()` 输出 `"C"` / `"Am"`。

### `ringbuf.rs`
rtrb SPSC 环形缓冲(生产者线程 → 音频回调)。

- `AudioRingBuffer { push, pop, available, split }`
- `push_samples` / `pop_samples` / `fill_with_silence_on_underrun`

### `pipeline.rs`
`AudioPipeline` — 引擎入口,拥有 producer 线程和 cpal stream。

拓扑:`CLI → [producer 线程: deck0 + deck1 → 混音 × 0.5] → ring → [cpal 回调]`

- `start(command_rx, response_tx)` — 初始化双 deck、预填静音、起流
- `handle_command` — 分发所有 Command(Load 起解码线程 / SetRate 调控制器 / ...)
- 音频回调零锁零分配,仅从 ring 读

### `deck/deck.rs`
`Deck` — 单 source、一流、非阻塞跳转。

- `play` / `pause` / `jump(target)` / `beatjump(beats)` — 传输
- `process_block(output)` — 每块先 `poll_ready_flows()` 切流,活跃流采样;暂停/耗尽补静音
- `set_analysis` — `ArcSwapOption` 无锁换入(分析可晚到)
- `set_ratio` / `set_profile` — 切活跃流的拉伸引擎
- **补偿逻辑**:jump 时记 `cued_from = current_frame()`,换流时新流定位 `flow.current + (current - cued_from)`,保证跨 deck 相位锁定

**`TimeShift`** — 后台预热线程,接收 Flow,调用 `prepare()` 后入库并通报 id。

### `flow/flow.rs`
`Flow` — 单次播放单元,包 `PitchShiftEngine`。

状态机:`Preparing → Ready → Active → Retired`

- `prepare()` — 调用引擎 `prepare_jump(start_frame)`
- `process_block` — 活跃时经引擎采样,非活跃补静音
- `set_ratio` / `set_profile` — 委托引擎(切 profile 会重建引擎并重新定位)

### `flow/pitchshift.rs`
`PitchShiftEngine` — timestretch-rs 引擎包装(Tape / Keylock / WideKeylock)。

- `process_block` — `feed(source → ring)` + `processor.process(output)`
- `prepare_jump(target)` — **Tape 直接重定位**(零预热);**Keylock/WideKeylock** 走 `reset → set_track_position → warm_start` 三步协议
- `set_ratio` → `controller.set_tempo_rate`(无 glide)

### `source/`
- `Source: { read_frames(start, output) -> usize, total_frames() }`
- `decoder::decode_file` — symphonia 解码,重采样到 48 kHz,合并立体声,后台线程调用
- `PcmPool` — 不可变 `Arc<Vec<f32>>` 的 `Source` 实现(克隆即 Arc bump)

---

## hypermixx-analysis
`lib.rs` — stratum-dsp → hypermixx 的适配层。

调用:**`stratum_dsp::analyze_audio(&[f32] mono, u32 sr, AnalysisConfig) -> Result<AnalysisResult>`**
调用:**`stratum_dsp::compute_confidence(&AnalysisResult) -> AnalysisConfidence`**

- `analyze(mono, sr)` — 调 `analyze_audio` + catch_unwind(兜 panic)+ `validate`(bpm>0 / 非空网格)
- `downmix_to_mono(source, total, channels)` — 交错立体声→单声道均值
- `AnalysisError` — `Stratum` / `Panicked` / `NoGrid`

转换:`AnalysisResult.beat_grid.beats`(秒)× sr → `BeatGrid.from_seconds`;`Key::Major/Minor` → `KeyReport`。

---

## hypermixx-cli
`main.rs` — 前端,解析命令、编排分析、格式化输出。

- `parse(line)` — 行→ `Vec<Command>`(load/play/pause/jump/beatjump/rate/profile/state/quit)
- `report(rx)` — 收响应;`Loaded.analyzed == false` 时返回 `needs_analysis=true`
- `spawn_analysis(deck_id, pipeline)` — 后台线程读 deck PCM → `analysis::analyze` → `SetAnalysis`
- `state` 触发 `GetAllStates`,awk 格式化为 table

---

## 外部依赖

### stratum-dsp(vendored `crates/stratum-dsp/`)
仅调用公开入口:
- `analyze_audio(mono_f32, sample_rate, AnalysisConfig) -> Result<AnalysisResult, AnalysisError>`
- `compute_confidence(&AnalysisResult) -> AnalysisConfidence`
- `AnalysisResult` 字段:`bpm`,`beat_grid.{downbeats, beats, bars}`(秒),`key: Key`,`key_confidence`,`grid_stability`

内部(HMM Viterbi 拍跟踪 / onset / chroma / key 检测)不直接调用。

### timestretch(vendored `crates/timestretch/`)
调用公开入口:
- `Engine::build(EngineConfig) -> Result<EngineHandles>` — 构造三件套
- `EngineConfig { sample_rate, channels, profile, initial_tempo_rate, max_block_frames, source_capacity_frames, pre_analysis }`
- `EngineProfile::Tape / Keylock / WideKeylock`
- `EngineProcessor::process(&mut out)` — 拉输出(零分配零锁)
- `EngineProcessor::reset()` / `warm_start_preroll_frames()`
- `EngineController::set_tempo_rate(f64)` / `warm_start(preroll)`
- `SourceProducer::push(interleaved)` / `set_track_position(frame)`

内部(SOLA / phase vocoder / varispeed / stage 链)不直接调用。

---

## 数据流

```
CLI  stdin
  │ crossbeam Command
  ▼
AudioPipeline ──producer thread──────────────────────────────────────┐
  │ Load:起 decode 线程→Deck::new(+ 可选常网格)                     │
  │       └─ 异步分析线程:Source→analysis::analyze→SetAnalysis       │
  │ Jump/BeatJump:记 cued_from,Flow 提交 TimeShift                   │
  │ TimeShift worker:Flow.prepare()→入库→mark_ready                  │
  │ 每 tick:poll_ready→switch_to(补偿)→active.process_block→mix      │
  └─ f32 ──→ ring ──→ cpal callback ──→ 声卡
```
