# Stem 方案（v2）：每条 stem 一条 flow

> v1 草案把 4 条 stem 在 `Source` 层求和，优点是零侵入，缺点是 per-stem `flow_fx` 不可能。
> 本版按 **每 stem 一条 flow** 重做：`flow_fx` / `flow_fader` 天然逐 stream，`deck_fx` 仍是
> 整个 deck 的（`channel.rs` 的注释本来就是这个设计意图）。代价是每 deck N 个 timestretch
> 引擎，以及 mixer 配置/协议要长出 stream 这一维。
>
> 测试素材：仓库根的 `stem-test.mp3`（9 656 340 帧 = 218.96 s @ 44.1 kHz 立体声）。

---

## P0 实测结果（2026-09-27，本机：16 核 / 15 GB，无 GPU，全程 CPU）

**结论：三个主要风险全部过关，方案可以按 §1 的「一个引擎 / 2N 声道」直接开工。**

### 分离后端（`charon-audio` 0.1.2 + `ort` 2.0.0-rc.13，HTDemucs 4-stem ONNX）

| 项 | 实测 |
|---|---|
| 构建 | ✅ `ort-sys` 静态链接 ORT 1.28.0（从 `cdn.pyke.io` 下载，无需系统库）；`charon-audio` 编译 14 s |
| 分离 3:39 曲目 | **78.9 s，RTF 0.360**（CPU EP，`shifts=1`，`htdemucs.onnx` 301.8 MB） |
| 峰值内存 | **1.90 GB**（结束时 2.08 GB） |
| **4 路帧长** | **全部 9 656 340 = 输入，逐帧等长 ✅**（这是原计划里的 1 号风险，干净过关，不需要 `AlignedSource` 裁齐） |
| 重建残差 `Σstems − input` | max\|err\| 0.156，rms 0.0119 → 相对输入 rms **−30.1 dB** |
| 模型获取 | ❌ `huggingface.co` / `cdn-lfs.huggingface.co` **不可达**；✅ `hf-mirror.com` 可达（~5 MB/s，302 MB 约 60 s），且 **SHA-256 = `68d0bf16…5e74` 与 charon 注册表钉死的哈希完全一致** |

两点值得单独记：

- **charon 故意不自动下载模型**（`model_zoo::download_model` 直接返回错误并给出 URL），所以「模型获取」天然就是我们的显式步骤 —— 正好跟计划里的「SHA-256 校验 + 缓存」一致。模型放 `~/.cache/hypermixx/models/htdemucs.onnx`，`SeparatorConfig::htdemucs(path)` 显式传路径。
- **残差 −30 dB** 意味着四路全 unity 时与原曲有可闻但不大的差别；「多存一条 residual」是可选项而非必需（§10.5）。

### 播放侧 CPU（关键设计决策的实测支撑）

用 `timestretch` 引擎按 `PitchShiftEngine` 的方式驱动（feed 到 `demand_hint`、256 帧回调），rate = 1.06：

| 配置 | RTF | 占单核（实时） |
|---|---|---|
| **1 × 8ch Keylock（4 stems 一个引擎）** | **20.6×** | **4.9%** |
| 4 × 2ch Keylock（4 stems 四个引擎） | 7.8× | 12.9% |
| 1 × 2ch Keylock（今天的单流 deck） | 31.2× | 3.2% |
| 1 × 8ch Tape（keylock off） | 64.0× | 1.6% |
| 4 × 2ch Tape | 22.1× | 4.5% |
| 1 × 8ch WideKeylock | 4.1× | **24.5%** |

- **一个 8 声道引擎比四个立体声引擎快 2.65×**（Keylock），Tape 下也快 2.94× —— §1 的决定得到实测支撑，而不只是相位一致性的论证。
- 绝对成本很小：**4 stems 只吃 ~4.9% 的一个核**；两个 deck 同时 stems ≈ 9.8%。即使 laptop 也绰绰有余。
- 4 stems（8ch）只比现在的单流 deck 贵 1.5×（4.9% vs 3.2%）—— keylock 的开销大头在**每引擎**而非每声道。
- **WideKeylock 是唯一需要盯的档位**：24.5%（一个 deck），两个 deck ≈ 49%。计划里 §7.2 关于「keylock wide 在 stem 模式下要谨慎」的备注保留。

复现：/tmp/stemspike（`src/main.rs` 分离测量，`src/bin/bench.rs` 引擎吞吐；stems 原始 f32 写在 `/tmp/stemfixture/{drums,bass,other,vocals}.f32`）。

---

## P1 完成（引擎与 deck 多声道）

已实现并全绿（192 个 lib 测试 + 全部集成测试）。**只动了 `flow/` 与 `deck/`，mixer 一行未改**
—— 所以这一阶段没有用户可见的行为变化（mixer 仍走 `pull_into` 的求和视图，per-stem 链路是 P2）。

| 改动 | 位置 |
|---|---|
| 一个引擎 N 路：`sources: Vec<Arc<dyn Source>>`、`channels = 2N`、交错 feed / 解交错 output、`render_discard` | `flow/pitchshift.rs` |
| `MAX_STREAMS = 4`（上游 8 声道上限 ÷ 2），超出返回带解释的 `InvalidFormat` | `flow/pitchshift.rs` |
| `Flow` 持单个引擎 + 共享 loop cell；`process_streams(&mut [Bus])` | `flow/flow.rs` |
| `Deck.sources` + `sources()` / `stream_count()`；`begin_block` / `render_streams` / `end_block` 三相位拆分（`process_block` / `pull_into` 保留为求和视图） | `deck/deck.rs` |
| `Deck::set_sources()`：在当前虚时钟位置做**换源 jump**，带上 loop range，并重建 armed LoopFlow | `deck/deck.rs` |
| `drive_loop_flow` 改用 `render_discard`（不再需要一块按声道数尺寸的 scratch） | `deck/deck.rs` |

新增的验收断言（都在 `cargo test -p hypermixx-audio` 里）：

- `one_eight_channel_engine_matches_four_stereo_engines` —— **逐位相等**（f32 上 `assert_eq!`）：
  一个 8 声道引擎的四路输出 == 四个独立 2 声道引擎。交错/解交错无损，unity 下两条路径完全等价。
- `four_streams_render_their_own_source_into_their_own_bus` —— 每路拿到自己的源，一个时钟。
- `one_loop_store_folds_every_stream` —— 一次 `set_loop_range` 四路同帧折返；断言**每一帧四路读的
  是同一个源帧**（这正是「四个引擎会漂」的那条性质）。
- `set_sources_swaps_to_four_streams_without_touching_the_clock` —— 播放中装 stems：路数 1→4、
  时钟连续、每路输出自己的源。
- `installing_stems_keeps_the_running_loop` —— loop 中换源：range 存活，跑满一圈后 audible 仍在区间内、
  slip 时钟越过 `out`。
- `the_mixed_view_of_a_stem_deck_is_the_sum_of_its_streams` —— 兼容路径仍是四路求和。

### 顺带查明的引擎契约（重要，免得再踩）

`feed()` 的 demand 是按 **`BLOCK_SIZE`（256）** 算的，所以 `process` 如果被传小于 256 帧，输出
**不对齐**：实测 32 帧一块时第二块从源帧 47.5 开始（`0..31` 然后 `47.5, 48.51, …`）。

- 这是**既有行为**，不是本次重构引入：新旧代码下 `process_block` 与 `process_streams` 给出完全相同的数字。
- 256 帧（mixer 的实际块长）下逐帧精确，loop wrap 也是（`…72, 73, 10, 11…`）。
- 结论：**驱动 transport 的单测必须用 `BLOCK_SIZE`**。已有那批 4/8/32 帧的小缓冲单测只能看第一块或只看
  end 语义；新加的测试一律用 `BLOCK_SIZE`。

### 有意未做

- **`AlignedSource`（长度裁齐）取消**：P0 已证明 demucs 四路逐帧等长，加这层是多余代码。真要防御
  应放在 `hypermixx-stems` 的安装侧，而不是播放路径。
- mixer 的 per-stream 链路（`Vec<FxChain>` / `Vec<Fader>` / `FxChainId::Stem`）= **P2**。


---

## 0. 信号路径（目标态）

```
per stream i (1 = mix, 4 = stems):
    deck.render_stream(i, sbus[i])
      → flow_fx[i]        逐 stem 插入   ← 你要的“每个 stem 单独 flowfx”
      → flow_fader[i]     逐 stem 电平
      → Σ 到 channel bus
    channel bus
      → deck_fx           整个 deck 共用   ← 你要的“整个 deck 用 deckfx”
      → deck_fader
      → crossfader
      → master / cue
```

现有注释已经写死了这个形状（`channel.rs:11-21`）：

```
flow_fx        逐流插入（换流即重置滤波记忆）
flow_fader     逐流电平；只有一个流时等于 deck 电平
deck_fx        逐 deck 音色（跨跳转持久）
deck_fader     1.0 until a track exposes stems
```

唯一要改的是把「一个 deck 一个流」放宽成「一个 deck N 个流」。

---

## 1. 核心结构：一个 `Flow` = 一个 N 声道引擎

**关键取舍**：不把 `Vec<Flow>` 改成 `Vec<Vec<Flow>>`（那会连带重写 `FlowShift` 的
supersede/reap 协议、`LoopArm`、`switch_to`、LoopFlow 同速驱动）；也**不是**「每条 stem 一个
引擎」，而是**一个 flow 一个引擎、声道数 = 2 × stream 数**（4 stem ⇒ 8 声道）。

理由主要不是省 CPU，而是**相位一致性**（详见 §7.1）：keylock 用 SOLA，它的拼接点是按
**全声道混合信号**做相关搜索得出的（`sola.rs::mix_channels` 预混所有声道，再对候选做点积；
读游标注释明写 "absolute fractional read cursor (shared: channels are lockstep)"）。
四个独立引擎实例会各自搜索各自的 stem、各自挑落点，读游标从此错开（`SEARCH_RANGE = 160`
帧 ≈ 3.6 ms，`DRIFT_TRIGGER = 192`），于是**四条 stem 不再相互对齐** —— 直接表现为 stem 之间
的梳状滤波和瞬态糊化，也就是「stems 加起来不再等于原曲」。同一个引擎的 N 个声道共享**一次**
拼接决策，所以四个 stem 永远同帧。附带的好处：搜索打分的其实是 sum(stems) ≈ 原混音，即
拼接决策退化成「普通 stereo 曲目会做的那个决策」。

```rust
// flow/pitchshift.rs —— 引擎持有 N 个源，内部交错成 2N 声道
pub struct PitchShiftEngine {
    controller: EngineController,
    processor: EngineProcessor,
    source_producer: SourceProducer,
    /// N 个源，每个已被 LoopSource 包过（仍是立体声 2 声道，`LoopSource` 无需改）
    sources: Vec<Arc<dyn Source>>,
    /// = 2 * sources.len()，传给 `EngineConfig.channels`（上游 validate 限定 1..=8）
    channels: usize,
    /// 交错读缓冲：FEED_CHUNK_FRAMES * channels
    feed_buf: Vec<f32>,
    /// 单条 stem 的暂存（交错前的连续立体声）：FEED_CHUNK_FRAMES * 2
    stem_buf: Vec<f32>,
    /// 解交错暂存：BLOCK_SIZE * channels
    out_buf: Vec<f32>,
    // track_position / output_frame* / ratio / total / priming_remaining / loop_cell 不变
}

impl PitchShiftEngine {
    pub fn with_profile(
        sources: Vec<Arc<dyn Source>>, ratio: f32, profile: EngineProfile,
        loop_cell: Arc<LoopRangeCell>,
    ) -> Result<Self, StretchError>;   // channels = 2 * sources.len()

    /// 逐 stem：`read_frames` → `stem_buf`（连续立体声）→ 散布进 `feed_buf` 的交错位
    /// `[i*channels + s*2 + c]`。`set_track_position` 每批一次（所有 stem 同一时间轴）。
    fn feed(&mut self);

    /// 解交错：`outs[s].l[i] = out_buf[i*channels + s*2]`，`.r[i] = +1`。
    pub fn process_streams(&mut self, outs: &mut [Bus], ctx: &FxContext) -> usize;

    /// 保留：`sources.len() == 1` 时的旧路径与旧测试。
    pub fn process_block(&mut self, output: &mut [f32]) -> usize;
    pub fn stream_count(&self) -> usize { self.sources.len() }
}
```

`Flow` 因此比草案**更简单** —— 只持有一个引擎：

```rust
pub struct Flow {
    pub id: u64,
    pub state: FlowState,
    pub start_frame: u64,
    pub end_frame: Option<u64>,
    engine: PitchShiftEngine,          // 一个，内含 N 路
    ready_tx: Option<Sender<u64>>,
    loop_cell: Arc<LoopRangeCell>,     // 仍由 N 个 LoopSource 共享
}
```

要点：

- **共享 `LoopRangeCell`**：`set_loop_range` 一次 store，N 个 `LoopSource` 同时折返，
  `loop edit` 只改一次。当前 `Flow::new_with` 是自建 cell，改成由参数传入。
- **只有一个时钟、一个读游标**：上一版担心的「四路漂移」不成立，N 路本来就是同一个
  `output_frame_exact` / 同一个 `virtual_frame()`。`current_frame` / `reached_end` /
  `set_ratio` / `reset_to` / `prepare` 全部照旧，不需要「遍历所有 stream」。
- **长度仍必须对齐**：短的一路会让 `feed()` 提前见底，budget / `reached_end` 分叉。安装时用
  media 里的 `AlignedSource { inner, frames }`（`total_frames()` 报统一长度、越界补零）包一层。
- **8 声道是硬上限**：上游 `EngineConfig::validate` 要求 `channels ∈ 1..=8`，所以 **4 stem
  恰好塞进一个引擎**；6 stem（12 声道）塞不下，只能拆成 8+4 两个引擎 —— 那时相位一致性只在
  引擎内部成立，两组之间又会漂。这是「选 4 stem 而不是 6 stem」的硬理由（见 §10.3）。
- 引擎数量：一个 flow 一个。跳跃期间 2 个 flow（warm 中 + 活动）⇒ 单 deck 瞬时 2 个引擎
  （8 声道 ×2），双 deck 4 个。ring 内存 32768 × channels × 4B ≈ 1 MB/引擎（8 声道）。

`FlowShift` / `LoopArm` / `switch_to` / `cued_from` 补偿 / LoopFlow 同速驱动**全部不动**——
它们只跟 flow 打交道，不关心里面有几路。

---

## 2. `Deck`：持有 N 个 source

```rust
pub struct Deck {
    sources: Vec<Arc<dyn Source>>,   // 原 `pool: Arc<dyn Source>`
    flows: Vec<Flow>,                // 不变
    active_index: usize,
    ...
}
```

改动点（都在 `deck/deck.rs`）：

| 现在 | 改成 |
|---|---|
| `Deck::new(pool)` | 保留 = `with_sources(vec![pool])`；新增 `with_sources(Vec<Shared>)` |
| `Deck::new` 里 `Flow::new_with(id, pool, ..)` | `Flow::new_with(id, self.sources.clone(), ..)` |
| `make_flow()` 里 `Arc::clone(&self.pool)` | `self.sources.clone()` |
| `total_frames()` = `self.pool.total_frames()` | `sources[0].total_frames()` |
| `source()` | `sources[0]`（分析/查询仍读干信号）；新增 `sources()` / `stream_count()` |

新增：

```rust
/// 换掉这一 deck 的源（安装 stems）。长度变化 ⇒ 按当前播放位置重建活动 flow，
/// 复用 jump 的 cued_from / 预热 / switch_to 机械，所以换源是无缝的。
pub fn set_sources(&mut self, sources: Vec<Arc<dyn Source>>) {
    self.sources = sources;
    let at = self.virtual_frame();          // 用虚拟（滑）时钟，与 jump 的补偿同一套
    let range = self.loop_range();          // 循环中换源必须把 range 带过去
    let flow = self.make_flow(at, range);
    self.cued_from = at;
    self.flowshift.submit_prepare(flow);
    // 若正 armed 一个 LoopFlow，同样按新源重建（它的引擎也必须换）
    if let Some(arm) = self.loop_arm.as_ref() {
        let p_in = arm.p_in;
        let provisional = self.loop_range();      // 见 loop_in()
        let flow = self.make_flow(at, provisional);
        // 走 submit_prepare_loop_flow，并更新 arm.id
    }
    self.dismiss_loop_arm_if_needed();
}
```

- 装 stems 是**一次换源 jump**，落点在 `virtual_frame()`。现有 `switch_to` 的
  `reset_to` + priming drain 已经保证换流不呼吸、无咔哒（`ARCHITECTURE.md` 已把它当契约）。
- **不需要** `ArcSwap` 热装、也不需要交叉淡化：换 flow 本身就是无缝的（且 loop range 语义
  天然正确）。这是 v2 比 v1 更干净的地方。
- 换源是异步的：命令到达后到 switch 落地之间，deck 仍在旧源上播放（最多几块）。因此
  **`stream_count()` 读的是活动 flow 的真实值**，`Channel` 必须按它（而不是按命令意图）
  决定自己的 stream 数。
- `begin_block()` 拆分：把 `poll_ready_flows + apply_sync` 从 `process_block` 里提出来，
  让 `Channel` 能在**同一块内**先让 switch 落地、再读 `stream_count()`、再决定是否
  resize。否则会出现「按旧路数渲染、新 flow 的第 4 路本块被丢掉」这类静默丢音。

```rust
pub fn begin_block(&mut self);                                  // poll_ready + apply_sync
pub fn render_stream(&mut self, i: usize, bus: &mut Bus, ctx) -> usize;
pub fn render_streams(&mut self, outs: &mut [Bus], ctx) -> usize;
pub fn end_block(&mut self);                                    // drive_loop_flow
pub fn process_block(&mut self, out: &mut [f32]) -> usize;       // = begin+sum+end，兼容旧调用
pub fn pull_into(&mut self, bus: &mut Bus, ctx) -> usize;        // 单路求和，兼容
pub fn pull_streams_into(&mut self, outs: &mut [Bus], ctx) -> usize; // Channel 走这个
```

---

## 3. `Channel`：N 条 flow 链 + N 个 flow fader

```rust
pub struct Channel {
    deck: Deck,
    flow_fx: Vec<FxChain>,       // 每 stream 一条（原 `flow_fx: FxChain`）
    flow_fader: Vec<Fader>,      // 每 stream 一个（原 `flow_fader: Fader`）
    stream_buses: Vec<Bus>,      // 每 stream 的 scratch
    bus: Bus,                    // 求和后
    cue: Bus,
    deck_fx: FxChain,            // 不变：整个 deck 共用
    deck_fader: Fader,
    crossfader: Fader,
    cue_send: Param,
    /// stream 数变化时按它重建 flow 链（`FxChain` 不是 Clone：FxSlot 持有 Box<dyn Fx>）
    flow_template: Vec<String>,
    cue_gain: f32,
}
```

`process` 重写：

```rust
pub fn process(&mut self, ctx: &FxContext) -> &mut Bus {
    self.deck.begin_block();
    let n = self.deck.stream_count();
    if n != self.flow_fx.len() { self.resize_streams(n); }   // 只在块边界、且只在数变时分配

    let mut bus = take(&mut self.bus); bus.ensure_frames(ctx.block_frames); bus.clear();
    // cue_send 是推进型平滑器：**一块只能 next 一次**，所以在这里取，不能塞进循环里的 take_cue
    let cue_gain = self.cue_send.next_block(ctx.block_frames, ctx.sample_rate);
    if self.cue_tap == CueTap::PostFlowFx { self.cue.clear(); }

    for i in 0..n {
        let mut sb = take(&mut self.stream_buses[i]);
        sb.ensure_frames(ctx.block_frames);
        self.deck.render_stream(i, &mut sb, ctx);
        if self.flow_fx[i].any_active() { self.flow_fx[i].process(&mut sb, ctx); }
        if self.cue_tap == CueTap::PostFlowFx { self.cue.add_scaled(&sb, cue_gain); }
        sb.scale(self.flow_fader[i].next_amp(ctx));
        bus.add_from(&sb);
        self.stream_buses[i] = sb;
    }
    if self.cue_tap == CueTap::PostFlowFader { self.take_cue_into(&bus, cue_gain); }

    if self.deck_fx.any_active() { self.deck_fx.process(&mut bus, ctx); }
    if self.cue_tap == CueTap::PostDeckFx { self.take_cue_into(&bus, cue_gain); }
    bus.scale(self.deck_fader.next_amp(ctx));
    if self.cue_tap == CueTap::PostDeckFader { self.take_cue_into(&bus, cue_gain); }
    bus.scale(self.side.gain_at(self.crossfader.next_position(ctx), self.curve));

    self.bus = bus;
    self.deck.end_block();
    &mut self.bus
}
```

- `take_cue_into` 改成接收 `cue_gain` 参数（`cue_send.next_block` 已提到外面只调用一次）。
  这是重写里最容易踩的坑：`Param::next_block` 调用 N 次会让平滑时间常数缩到 1/N。
- **cue tap 语义**（N 路下的定义，写进文档）：`PostFlowFx` = Σ 各路 flow_fx 之后的信号；
  `PostFlowFader` = Σ 各路 flow_fader 之后；`PostDeckFx/Fader` 不变（单总线）。
- `slot()` / `locate_slot()` / `slot_statuses()` / `add_slot()` 的「合并索引空间」在 N 路下
  不再成立。**改成显式寻址**：`SlotChain::Flow(Stem)`（每路一条链）+ `SlotChain::Deck`。
  `FxChainId::Deck(d)` 从此只指 deck 链（原本 `flow_fx` 为空时合并空间等价于 deck 链，
  所以默认拓扑下行为不变；CLI/completer 要跟着改）。

### 配置语义

`ChannelConfig.flow_fx: Vec<String>` 从「一条链」变成「**模板**：每条 stream 都照它建一条」。
- mixer 构建时 channel 只有 1 路（`Deck::empty()`），所以建 1 条。
- 路数增长时用模板**重建**（不能 clone：`FxSlot` 持有 `Box<dyn Fx>`）。
- 因此 `MixerConfig::build_chains` 的返回从 `Vec<(Vec<FxSlot>, Vec<FxSlot>)>` 变成
  `Vec<(Vec<String>, Vec<FxSlot>)>`（flow 侧留名字，构建推迟到路数已知时）。
- 运行时把某条 stream 的链改花了，再由换轨/换源触发 resize 会回到模板——可接受（换轨本来就
  重置 deck），文档写明。

### 每 stem 的 level / mute / solo

复用 `flow_fader`，不引入第二套增益级：

- `mute` = 把该路 flow fader 打到 `-1.0`。`bipolar_amp(-1.0) == 0.0` 是**精确定义**的
  （`dsp.rs:331`，且 `a_closed_fader_is_silent` 已钉住），所以 mute 是数字静音，不是 -80dB 泄漏。
- `solo` = 集合语义：`solo_set` 非空时，未选中的路 mute（`Channel` 里存
  `stem_level: [f32;4]` / `stem_mute: [bool;4]` / `stem_solo: u8`，每次变更重算 4 个 fader 目标）。
- `preset`：`Acapella` = 只留 vocals；`Instrumental` = mute vocals；`DrumsOnly` / `BassOnly` 同理。
- 电平用**双极性位置**（与 `SetFader` 同域）。若前端想用 0..1 幅度，在 `dsp.rs` 的
  `bipolar_amp` 旁边加一个逆函数 `bipolar_from_amp`（分段可逆：`amp>1 → 20·log10/16`；
  `amp<=1 → sqrt(amp)-1`，`amp<=1e-4 → -1`）。

---

## 4. 协议（`core`）

```rust
// core/src/stem.rs（新增）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Stem { Drums, Bass, Other, Vocals }          // 数组下标 = demucs 输出序
impl Stem { pub const ALL: [Stem; 4]; pub fn name(self) -> &'static str; pub fn parse(&str) -> Option<Self>; }

/// 安装用载荷：固定 4 路等长 PCM。
pub struct StemSet { pub stems: [Shared; 4] }

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StemStatus {          // 回给前端（TUI 徽标 / `stem status`）
    pub ready: bool,
    pub level: [f32; 4],         // 双极性位置
    pub mute: [bool; 4],
    pub solo: u8,                // 4 bit
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StemOp {
    Level { stem: Stem, position: f32 },
    Mute { stem: Stem, on: bool },
    Solo { stem: Stem, on: bool },
    Clear,
    Preset(StemPreset),
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StemPreset { Full, Acapella, Instrumental, DrumsOnly, BassOnly }
```

命令与寻址：

```rust
// command.rs
Command::SetStems { deck_id: DeckId, stems: StemSet },  // 换源 + 重建 flow（异步无缝）
Command::Stem { deck_id: DeckId, op: StemOp },          // mixer 侧：改 flow fader / solo / preset

pub enum FxChainId { Deck(DeckId), Stem { deck: DeckId, stem: Stem }, Master }
impl FxChainId { pub fn label(&self) -> String }         // "deck0" / "deck0/vocals" / "master"

pub enum FaderTarget {
    /// 单流 deck 的 flow fader；有 stems 时 = 一次写全部 4 路（MIDI 一个推子仍然好用）
    Flow(DeckId),
    /// 一条 stem 的 flow fader
    Stem { deck: DeckId, stem: Stem },
    Deck(DeckId), CueSend(DeckId), Crossfader, Master, Cue,
}

// DeckState
pub stems: StemStatus,
```

`route()`（`pipeline.rs:352`）：

```rust
SetStems { deck_id, stems } => answered(mixer.set_stems(deck_id, stems)),
Stem { deck_id, op }        => answered(mixer.apply_stem(deck_id, op)),
FaderTarget::Stem { .. }    => /* 同 set_fader 的路径，落到 Channel::set_stem_fader */
```

`Mixer::set_stems` 落到 `Channel::set_stems` = `deck.set_sources(...)` + 记录 `pending_stems`
（供 `stems.ready`）。`Channel::resize_streams` 在流数增加时把 `ready` 置真。

`state_of` 从 `(deck_id, deck, sync)` 改成 `(deck_id, channel, sync)`，`stems` 来自
`channel.stem_status()`。连带要改：`tui/app.rs:130`（乐观更新）、`response.rs:274`（测试）、
`tests/common/mod.rs` + `loop.rs`。让编译器点名即可。

---

## 5. 前端语法

CLI 是 target-first（`command.rs:208/724`），所以在 deck 之后加一层 stem 子目标最自然：

```
deck0 stem separate            # 触发离线分离（worker + 进度 notice）
deck0 stem status
deck0 vocals fx add filter     # 只给 vocals 这条 flow 链加 filter   ← per-stem flowfx
deck0 vocals fader 0.5         # 该 stem 的 flow fader
deck0 vocals mute | unmute | solo | unsolo
deck0 stem acapella | instrumental | drums | bass | full
deck0 fx add eq                # deck 链（整个 deck 共用）            ← deckfx
deck1 stem separate --model htdemucs_ft --shifts 1
```

实现：`parse_target` 消费 `deck0` 后，若下一个 token ∈ `{vocals,drums,bass,other,stem}`，
把 `Target::Deck` 升级为 stem 子目标（或 `Target::Stems(deck)`）。`SlotBook` 的 key 用
`FxChainId::label()`（`deck0/vocals`）就能复用现有补全。

TUI：deck 头行加 4 格（电平条 + `M`/`S` 徽标），分离进度走 `NoticeTx`；`waveform_view`
以后可以按 solo 的 stem 画峰值（v2+，需要 per-stem `Waveform`）。

MIDI：`StemOp` 在 `translate.rs` 里映射（纯函数，单测友好）；4 个 pad = mute 切换，
编码器 = 电平，演出实用。`midi-map.toml` 补默认。

---

## 6. 分离后端（沿用 v1 结论，独立于本方案的播放架构）

`hypermixx-stems`（新 crate，依赖 `core` + `media`，与 `library` 平级，把 ML 重依赖隔离在
`stratum-dsp` 之外）：

```rust
pub trait StemSeparator: Send {
    fn id(&self) -> &str;                       // 进缓存 key
    fn separate(&self, mix: &dyn Source, progress: &mut dyn FnMut(f32)) -> Result<StemSet, StemError>;
    fn cancel(&self);
}
pub struct MockSeparator;   // 必须项：CI 与离线开发都靠它
```

- 模型：HTDemucs（4 或 6 stem），原生 **44.1 kHz = 引擎率**，全链路零重采样。
  **实测已确认 4 路逐帧等长**，不需要 `AlignedSource`（§P0）。
- **模型获取是显式步骤，不是后端自动下载**：`huggingface.co` 在本环境不可达，走
  `hf-mirror.com`（或本地文件）；按 charon 注册表里钉死的 SHA-256 校验后再落到
  `~/.cache/hypermixx/models/`。这条要写进我们自己的 crate，不要依赖后端的下载器。
- 后端候选（都年轻，必须 P0 spike 验证）：`charon-audio` 0.1.2（API 最合：内存 `Stems`、
  `separate_to` 有界内存、region 分离；但 2026-09-26 才发版、下载 ~400）；`stem-splitter-core`
  1.2.0（只输出**文件路径**，要再解码；默认 feature 会拉 CUDA）；`demucs-rs`（Burn，自认
  macOS-heavy）；**子进程**（引擎不链接 300MB ML 运行时，最稳，先跑通）。
- 缓存：`~/.cache/hypermixx/stems/<sha256(model‖源哈希‖sr)>/`，裸 f32 + TOML 头；命中即装。
- 内存：7 min 曲目单条 ≈ 148 MB，4 条 ≈ 600 MB。因为本方案 4 路**同时**在内存里，这条比 v1
  更疼：必须提供 `MmapSource`（mmap 缓存文件当 `Source`）作为一等公民，而不是可选项。
- 分离线程 `Drop` 时若 deck 已被换轨，结果只落缓存、不安装（安装前校验仍是同一个 track）。

---

## P2 + P3 完成（逐 stem 链路与真后端）

已实现并全绿（workspace 全部测试 + 真机端到端）。**mixer 现在是逐 stream 的**，`huggingface` 之外
的一切都落地了。

| 改动 | 位置 |
|---|---|
| `stem` 类型：`Stem` / `StemSet` / `StemStatus` / `StemOp` / `StemPreset`（`Stem::index()` 即引擎 stream 序号） | `core/stem.rs` |
| `Command::SetStems` / `Command::Stem` / `Command::GetStemState`；`CommandResponse::Stems`；`FxChainId::Stem{deck,stem}`；`FaderTarget::Stem`；`DeckState.stems` | `core/command.rs`, `core/deck.rs` |
| `Channel.flow_fx: Vec<FxChain>` / `flow_fader: Vec<Fader>` / `stream_buses: Vec<Bus>`；逐 stream 渲染；`SlotChain::Flow(Stem)` 取代**合并索引空间** | `mixer/channel.rs` |
| 逐 stem 意图（level / mute / solo 三件独立事实）→ 一处合成 fader 位置；mute = `-1.0`（`bipolar_amp` 的精确静音） | `mixer/channel.rs` |
| `Mixer::set_stems` / `apply_stem`；`resolve_fx`/`add_fx`/`remove_fx`/`list_fx` 按 chain 寻址 | `mixer/mod.rs` |
| `hypermixx-stems`：`StemSeparator` trait + `MockSeparator` + 模型表/镜像下载/SHA-256 + 磁盘缓存 + `CharonSeparator` | 新 crate |
| CLI：`stem` 族 + stem 子目标（`deck0 vocals …`）+ 后台分离 worker + 进度 + 缓存 | `cli/command.rs` |
| 渲染：`stems_badge`（CLI 与 TUI 共用）、`stem_report`（`stem status`）、TUI 逐 stem 彩条、补全 | `cli/response.rs`, `cli/tui/*` |
| `fx/mod.rs` 导出 `bipolar_amp` | 前端要把电平显示成 dB，就必须和 mixer 用同一条 fader 律 |

### 端到端实测（`stem-test.mp3`，3:39）

```
load stem-test.mp3 122          → 9656340 frames
deck0 stem separate             → 88 s，逐帧 9656340，缓存 294.7 MB
（重跑）deck0 stem separate      → 0 s（缓存命中）
deck0 stem acapella             → D/B/O muted，V audible
deck0 vocals level -0.5         → V -0.5，-12 dB
deck0 vocals solo on            → solo 覆盖 mute：V-0.5S
deck0 vocals fx add filter      → deck0/vocals fx[0]，与 deck 链索引空间完全独立
deck0 beatjump 64 / loop 8 / stem separate → 循环中换源，loop [1474820-1648328] 存活
```

### 两处设计上值得记住的地方

- **`DeckSide::Center` + `flow_fader` 逐 stream** 让单流路径逐位不变（原有 192 个测试一行未改），
  所以"加了 stems"没有给普通曲目带来任何行为变化。
- **合并索引空间被删掉了**：一条链一个 stream 之后，"deck 的 slot 2" 可以指四个不同的效果。
  `FxChainId::Deck` 从此只指共享 deck 链，代价是 CLI 里 `deck0 vocals fx …` 必须写全（`fx help`
  与补全都跟着改了）。

## 7. 成本与风险

1. **stem 之间的相位一致性（最高风险，已由 §1 的结构解决）**：SOLA 的拼接点按全声道混合信号
   搜索（`SEARCH_RANGE = 160` 帧 ≈ 3.6 ms，`DRIFT_TRIGGER = 192`）。**若每条 stem 各开一个
   引擎，四个实例各自挑落点、读游标从此错开，stem 之间就开始梳状滤波** —— 这不是性能问题，
   是「stems 加起来不再等于原曲」的正确性问题。所以必须一个引擎 N 声道；回归测试里要把这条
   钉住（4 个独立 2 声道引擎的输出对齐后会看到偏差，1 个 8 声道引擎不会）。
2. **CPU：4× 的逐声道工作量不可消除**（它就是 4 倍音频）。合并引擎省掉的是：SOLA 的相关搜索
   （按混合信号做，cost 与声道数无关；4 个引擎 = 4 次搜索）、`mix_channels` 预混、以及
   N-1 次 build / ring / Vec 分配 / `prepare_jump` + `drain_priming`。收益最大的其实是
   **预热与跳跃延迟**：`FlowShift` 单线程预热，Keylock 的 preroll 是 `pipeline_latency + settle`，
   WideKeylock 是 `2 × WIDE_FFT`；4 个引擎就是 4 倍预热时间，会吃掉 `cued_from` 的补偿窗口。
   缓解：keylock off 时 `build_stages(Tape) == vec![]`，整条 keylock 链消失、只剩 varispeed
   重采样，几乎免费。**实测（§P0）：一个 8 声道 Keylock 引擎 = 4.9% 单核，四个 2 声道引擎
   = 12.9%；WideKeylock = 24.5%。**
3. **四路长度一致性**：`AlignedSource` 结构性保证；demucs 的 pad/trim 在 P0 量化。
4. **换源是异步的**：命令到达 → 落地之间有几块仍在旧源。`begin_block()` 拆分是为了让
   `Channel` 按真实 `stream_count()` resize；漏掉就会静默丢一路。
5. **cue tap 与 `Param::next_block` 次数**：见 §3，重写时的两个具体陷阱。
6. **协议面变宽**：`FxChainId` / `FaderTarget` / `SlotChain` / `ChannelConfig.build_chains` /
   `DeckState` 全都要动，`Channel` 的合并索引空间被拆掉。编译器会点全名，但 CLI 的 `fx`
   寻址与补全要跟着改（默认拓扑下 deck 链的索引不变，所以脚本多数不受影响）。
7. **模型/后端不成熟**：见 §6，后端 trait + feature gate + 子进程兜底，保证
   `cargo build --workspace` 永远能过。

---

## 8. 分阶段

| 阶段 | 内容 | 验收 | 估计 |
|---|---|---|---|
| **P0 spike** | 用真实分离器跑 `stem-test.mp3`：量耗时/峰值内存；断言 4 路**逐帧等长**；`Σstems` 与原轨残差量级；顺便用 4 条 wav 手动拼一个 4 路 flow 压测 CPU | 选定后端 + 一页数据；不合并生产代码 | 0.5–1 d |
| **P1 Engine/Deck 多声道** ✅ | `PitchShiftEngine` 持 `Vec<Source>` + `channels = 2N`；`Flow` 单引擎 + 共享 loop cell；`Deck.sources` / `begin_block` / `render_streams` / `set_sources` | ✅ 完成（见上）：逐位等价断言 + 四路路由 + loop 折返 + 换源保 loop，192 lib 测试全绿 | 2–3 d |
| **P2 Channel 多链** | `Vec<FxChain>` / `Vec<Fader>` / `stream_buses`；cue 语义；config 模板；`SlotChain::Flow(Stem)` | `deck0 vocals fx add filter` 只影响该路；`deck0 fx add eq` 影响整 deck；mute/solo 代数 | 1.5–2 d |
| **P3 真后端 + 命令面** | `hypermixx-stems` + 缓存 + mmap + 进度/取消；`SetStems`/`Stem`/`FxChainId::Stem`/`FaderTarget::Stem`/`DeckState.stems` | `deck0 stem separate` → 播放中切 acapella/instrumental | 2 d |
| **P4 UX** | CLI 语法 + TUI 4 格电平/徽标 + 补全 + MIDI + 文档 | 演出可用 | 1 d |
| **P5（可选）** | per-stem 波形、6-stem、短窗实时分离、per-stem 发送到独立输出 | — | — |

---

## 9. 测试（素材 `stem-test.mp3`）

- **Mock 是必须项**：`MockSeparator` 两种模式——「全给 stem 0，其余静音」用于验证路由与
  mute/solo；「每路都是 mix，fader 各 1/4」用于验证求和。不依赖任何 ML 依赖，CI 可跑。
- `Flow` 多路：8 声道引擎 4 路输出分别等于各自的源（用 ramp 源，值即帧号）；与 4 个独立
  2 声道引擎在 ratio=1.0 下逐位一致；共享 loop cell 下一次 `set_loop_range` 四路同时折返。
- 相位一致性回归：ratio ≠ 1.0 且 keylock on 时，把 4 条 stem 相加与原混音做相关，断言
  互相关峰值始终在 lag 0（这条测试就是「不能每条 stem 一个引擎」的证据）。
- `Deck`：`load` → `SetStems` → 帧号连续、无咔哒（断言换源前后一块的样值差有界）；
  换源在 loop 中发生 ⇒ range 存续；armed LoopFlow 换源后仍能在 `out` 时原地转正。
- `Channel`：`deck0 vocals fx add filter` 只改一路（另外三路逐位不变）；`deck_fx` 改四路；
  `mute` 后该路**逐位为 0**；`solo vocals` 后仅 vocals 非零；`cue_send.next_block` 每块只
  调用一次（用 `is_moving`/收敛速度间接断言）。
- 端到端：`stem-test.mp3` 真实分离 → 4 路播放 → 与原轨 A/B；P0 的等长/残差断言固化成测试。
- 回归：现有 `engine` / `beatlock` / `loop` / `sync` / `tone_faithful` 必须全绿
  （单流路径逐一行为不变）。

---

## 10. 开放问题

1. ~~CPU 预算~~ **已实测回答（§P0）**：4 stems 一个 8 声道 Keylock 引擎 = 4.9% 单核，
   两 deck 同时 stems ≈ 9.8%，**可以接受**；唯一要盯的是 WideKeylock（24.5%/deck）。
2. `FaderTarget::Flow(deck)` 在有 stems 时的语义：一次写 4 路（我倾向这个）还是报错要求
   显式指定 stem？
3. stem 路数固定 4，还是允许 6（guitar/piano）？注意 **8 声道是引擎硬上限**：6 stem = 12
   声道，只能拆成 8+4 两个引擎，组间相位会漂（§7.1）。若坚持 6，要么接受组间漂移，要么改
   上游 `validate` 的上限（那是别人的 git 依赖）。
4. 缓存落盘格式：裸 f32（快、600MB/曲）vs 16-bit（省一半、有量化噪声）？是否 mmap 作为默认？
5. 是否保留一条 `residual = mix − Σstems`，让四路全 unity 时逐位还原原曲？
   **实测残差是 −30.1 dB**（§P0），可闻但不明显 —— 可以先不做，留成配置项。
