# HyperMixx 架构详解（学习指南）

> 这份文档讲**为什么这样设计、数据怎么流**——读完你能回答：
> 「引擎和 UI 怎么通信的？」「为什么播放跳转没有延迟？」「loop 为什么是状态机？」
> 上手操作（构建/测试/运行命令）见根目录 `README.md`，设计决策的逐条论证与开发史见 `实现方案.md`。

## 1. 整体：四个 Rust crate + 一个 Flutter 壳

```
┌────────────────────────────── Flutter（UI 线程）──────────────────────────────┐
│  lib/engine/   控制器：60Hz 快照轮询、分析事件订阅、命令下发                      │
│  lib/painters/ 波形/播放头/overview 绘制                                         │
│  lib/widgets/  deck 面板、transport、loop、FX 等组件                              │
└──────────────┬──────────────────────────────────────┬──────────────────────────┘
               │ flutter_rust_bridge（同步调用 + 事件流）│
┌──────────────▼──────────────┐  ┌─────────────────────▼─────────────────────────┐
│  hypermixx-bridge（桥）      │  │  hypermixx-analysis（分析线程 ×deck）            │
│  FRB 注解面 + 静态引擎单例    │◄─┤  独立解码 → 渐进波形 → BPM/调性 → 能量信封        │
└──────────────┬──────────────┘  └─────────────────────┬─────────────────────────┘
               │ EngineOp 队列 + 总线                    │ AnalysisEvent 事件
┌──────────────▼───────────────────────────────────────▼─────────────────────────┐
│  hypermixx-audio（引擎）                                                         │
│  cpal 音频回调线程：EngineState::process（每块 5.3ms @48kHz/256帧）                │
│    └─ Deck ×2：参数快照 → sync → 处理链（keylock 变调不变速）→ loop 环喂入          │
│       └─ TrackCache（filler 线程 ×deck：磁盘 → 全曲预解码内存缓存）                │
└──────────────┬──────────────────────────────────────────────────────────────────┘
               │
┌──────────────▼──────────────────────────────────────────────────────────────────┐
│  hypermixx-core：控制总线（ControlBus）、拍钟、beatgrid、路径常量。无 IO、纯数据。  │
└─────────────────────────────────────────────────────────────────────────────────┘
```

依赖链：`core ← audio ← analysis / bridge`。core 是最小公共层，audio 是核心，analysis 独立于 audio 跑（只有 bus 和 TrackCache 的弱引用表是共享的），bridge 是把 audio 暴露给 Flutter 的胶水。

**为什么按实时性分层**——音频回调线程是整栋楼的「不可破坏」约束：

| 约束 | 原因 | 对应设计 |
|---|---|---|
| 回调内不能阻塞 | 阻塞 → 声卡欠载 → 爆音/断流 | 读参数走 seqlock（无锁）、操作走 try_lock 队列、数据走预解码内存 |
| 回调内不能分配 | 分配可能触发系统调用/锁 | TrackCache 分块 `OnceLock<Box<[f32]>>` 预先划分，copy 零分配 |
| 回调内不能做 IO | 磁盘/网络延迟不可控 | 解码全部在 filler 线程，回调只读内存 |
| 每块预算固定（256 帧 ≈ 5.3ms） | 超时即断流 | 全曲预解码让 seek/loop 都是纯内存操作 |

## 2. 三条数据流（先看这一节）

### 2.1 控制流：UI 与引擎只通过一条总线

`ControlBus`（Mixxx ControlObject 思想的 Rust 版）是**唯一的通信面**。每个控制点是一个字符串路径（`"Deck1.play"`、`"Master.crossfader"`），持有者是 `ControlHandle`（Arc 克隆，任何线程可拿）。

- **UI → 引擎**（参数）：按钮/推子 set 到总线；引擎每块开头 `update_params()` 批量快照
- **引擎 → UI**（显示）：引擎把 playhead、vu、bpm、缓存进度写总线；UI 每 16ms 一次 `snapshotAll()` 同步调用取全部快照，驱动界面（60Hz 视觉刷新率，D7 定案——单 Timer + 单次调用，不做 N 次小调用）
- **引擎 → 引擎**（动作）：需要语义而非数值的操作（载曲、seek、beatjump、设 loop）走 `EngineOp` 队列——UI push，音频回调在**块边界**消费（`try_lock`，绝不与 UI 线程互斥等待）

为什么不用互斥锁存参数？因为「UI 写、音频读」的竞争频率极高且读侧延迟敏感——seqlock 让读侧完全无锁（见 §3.1）。

### 2.2 音频流：文件 → 内存 → 处理链 → 混音 → 声卡

```
load_track ──► TrackCache.open：起 filler 线程，曲首开始顺序解码到内存（2048 帧/块）
                │ 播放头推进时 request_priority(播放位置)（跳填，播放头优先）
                ▼
Deck::process：copy_ready(缓存, 当前帧, 256帧)  ← 环内则按 loop 状态机从环上取
                │（缓存未填到会欠载，回退 rfft 解码直读——但 16KB/块的内存 IO
                │  远比磁盘解码快，正常情况永远走缓存）
                ▼
keylock：变调不变速（timestretch 0.11），含 pitch 移调；≈560 帧延迟，显示侧补偿
                ▼
EQ/滤波/FX 槽（每 deck）→ ×crossfader 因子（逐采样平滑）→ master 音量 → tanh 软限幅 → 声卡
```

**关键：seek/beatjump/loop 为什么无延迟？** 全部命中预解码内存，跳转就是改一个读取偏移 + 最小预卷（warm_start），没有磁盘等待。这是全项目最重要的架构决策（P2 立项即定，P23 做深）。

### 2.3 事件流：分析线程 → 桥 → Flutter

```
load_track ──► 起独立分析线程（不占 TrackCache 的 filler 读带宽）
    ├─ 渐进：先出 Segment 事件（渐进波形，播放头附近优先分析，P9）
    ├─ 完事：TrackAnalysis 事件（完整 255 列波形 + beatgrid + tempo_segments + 能量信封）
    └─ 任何新载曲 → generation+1 → 旧线程 shutdown，旧事件全部作废（桥按代过滤）
                 ▼
bridge::forward_events 线程：事件 → 把 grid_bpm/grid_offset 写总线（引擎实时用）
                              + 转发 StreamSink → Dart 侧订阅 → wave_model 刷新
```

## 3. 核心机制逐个拆

### 3.1 ControlBus（core/control.rs，120 行）

- 每个控制点 = `SeqLock<f64>` + `generation: AtomicU64`
- 读侧 `seqlock` 完全无锁；写侧仅在值变化时才写（避免 UI 轮询写同值空转、无谓代次递增——UI 控件每个 tick 都会 set 一遍）
- `generation` 计数供 UI 判断「值是否变过」，widget 可据此跳过重绘
- `paths` 模块集中定义路径字符串（`core/src/lib.rs`），两侧不再写魔法字符串，有稳定性测试防误改
- 注册表用字符串而非 enum：**扩展性**——MIDI 映射、脚本化、曲线图记录都要按名字寻址控制点，字符串是公共接口语言

### 3.2 TrackCache：全曲预解码（audio/track_cache.rs）

- 预分配 `CHUNK_COUNT` 个 `OnceLock<Box<[f32]>>`（2048 帧 ×2 声道 ×4B = 16KB/块）；`OnceLock` 语义 = 每块**只被 filler 写一次**、读侧 `get()` 零锁零分配
- filler 线程循环：顺序填充（曲首优先）→ 每轮检查 `priority` 寄存器（播放头/seek 目标）跳填 → EOF 后回补最低洞
- 读侧三个判定：`copy_ready`（拷走 n 帧，返回实际可用）、`range_ready`（欠载判别）、`filled/total`（进度显示总线 `Deck{}.cache_filled`）
- 全局弱引用注册表：重复载入同一路径复用缓存，曲目切换时旧缓存被 Arc 引用者自然回收
- 测试友好：`test_set_chunk/test_set_total/test_set_eof/test_set_filled` 四个助手让单元测试免建真实文件

### 3.3 Deck 实时处理链（audio/deck.rs，~1600 行，全项目最复杂的文件）

每块（256 帧）依次：

1. **`update_params`**：从总线批量快照参数（play/rate/grid/keylock/loop 等），值变化才做重配置（如 timestretch 参数、EQ 系数），未变则零开销
2. **`apply_sync`**（follower）：P14 定案的「一次性对齐 + 速率锁」——开启沿做一次相位对齐，之后只锁速率（follower bpm = leader grid × 滑杆），速率连续无跳变；推子/nudge 软接管（±0.5% 点带防触摸跳变）
3. **`process`**：状态机分派 → 引擎路径（`process_engine`，loop 环喂入）或线性路径
4. 输出写 `tmp` 缓冲 → 引擎侧混音（`crossfade_factors` 逐采样平滑 + master）

**loop 状态机**（P23 核心，deck.rs:562 一带注释是入口）——为什么是状态机而不是简单的「播放到 out 回 in」：因为音频是流式的，切换必须发生在**圈界对齐**时刻，且要避免咔嗒：

```
loop_active 上升沿 → init_loop_ring：
    entry blend（首圈 d 到 d+blend 渐变，防起圈爆音）
    loop_offset_engage = 位置不在环起点时（环从当前位置经 wrap 进入）
loop_ring = true → feed_pos 模环长回绕（圈界 wrap blend，P11）
loop_active 下降沿 → loop_exiting = true：喂完当前圈（收尾圈），
    finish_loop_ring：锚定退出点 + 切线性续喂——关环不掉拍
环内 beatjump/seek → 外部跳转：环取消、状态清位（P14 修正）
```

loop 起终点（ManualLoop In/Out）UI 只传 raw 播放头秒数，量化（snap 拍线、保底 1 拍、无起点回拉）全部在引擎侧 `snap_loop_bounds`（deck.rs:517）完成——**量化语义在引擎，UI 只做透传**，这是 P23 定的边界。

**keylock 管线**：`TimestretchLocker`（timestretch 0.11 crate）把「变速后的音频」变调回原调。管线固定 ≈560 帧延迟，所以 playhead 显示 = 实际位置 − 延迟（显示补偿，P22-C 动态延迟契约），否则波形会对不上声音。

**beatjump**（P16/P17 反复后定案）：跳距 = N × 60 / bpm（**精确整拍，不做落点吸附**——吸附反而让跳距飘），配合 warm_start 最小预卷消掉跳转 33ms 静音（P14）。

### 3.4 分析管线（analysis/）

- 5 个文件职责清晰：`mono.rs`（解码 → 单声道）、`waveform.rs`（分列能量 → 255 列波形）、`segment.rs`（BPM/调性/beatgrid + `TrackAnalysisData` 聚合）、`energy.rs`（独立能量信封，hop 512 帧 RMS）、`lib.rs`（线程编排 + 事件类型）
- `refine_grid_rigid` 采纳时会把多段 grid 合并成单一 rigid 段——所以 `tempo_segments`（分段 BPM 表，供 Flutter 拍轴显示）必须在 refine **之前**捕获（segment.rs 的实现顺序就是设计顺序）
- 分析是**独立解码**（不读 TrackCache），因为它的访问模式是「从头到尾渐进扫」，与 filler 的「播放头优先」互相竞争会两败俱伤；代价是同一文件双份解码内存，换来两线程互不干扰

### 3.5 FRB 桥（bridge/）

- `api.rs` 是 FRB 注解面（#[frb] 注解的类型与函数，**改它要 regen**），`bridge.rs` 是实现——改动面被分成「会改 Dart 绑定」和「不会」两层
- **静态单例** `CORE: Mutex<Option<Core>>`：FRB 没有对象生命周期跨语言管理，桥内自持有 bus + engine handle + 后端，生命周期即进程
- 双向通道：同步（`bus_set/bus_get/snapshot_all` 立即返回）+ 异步（`StreamSink<AnalysisEventWire>` 事件流）；forward_events 线程把分析事件拆两路——写总线（引擎需要的 grid）+ 转发 Dart（UI 需要的波形）
- 快照 wire 类型（`DeckSnapshotWire`）是**总线的一次批量拷贝**：UI 永远读「最近一次完整快照」，不逐点跨 FFI——单次调用拿到一切

### 3.6 Flutter 侧

- `engine/engine_controller.dart`：启动引擎 + `Timer.periodic(16ms)` → 每次 `snapshotAll()` 一次 FFI 调用 → 分发到 DeckController；桥不可用（widget 测试环境）时不启动 Timer
- `engine/wave_model.dart` + `painters/`：波形数据在 Dart 侧持有（由分析事件流写入），painter 只画不存
- `widgets/`：`hypermixx_screen.dart` 是根布局，`deck_panel.dart` 组装左/中/右区（wave/transport/pads/fx/tempo），其余组件各自独立文件（manual_loop、beatjump_panel、tempo_fader、transport_row……）
- widget 测试全部**注入假动作**（`Actions` 抽象接口），不碰桥、不起引擎——UI 逻辑与引擎逻辑在测试里彻底解耦

## 4. 关键文件地图（按阅读顺序）

| 文件 | 是什么 | 先看哪个符号 |
|---|---|---|
| `core/src/control.rs` | 控制总线（60 行核心） | `Control::set`（值未变跳过写） |
| `core/src/lib.rs` | 路径常量 | `paths` 模块 |
| `audio/src/engine.rs` | 音频回调入口 | `EngineState::process`（3 步：op 队列→deck 处理→混音） |
| `audio/src/deck.rs` | deck 状态机（最难） | `process` → `init_loop_ring` → `apply_sync` |
| `audio/src/track_cache.rs` | 预解码缓存 | `filler_main`（线程主循环）→ `copy_ready` |
| `audio/src/keylocker.rs` | 变调不变速 | `TimestretchLocker::build` |
| `audio/src/backend.rs` | cpal 后端抽象 | `AudioBackend` trait |
| `analysis/src/segment.rs` | BPM/网格/聚合 | `refine_grid_rigid` 前后（tempo_segments 捕获） |
| `analysis/src/lib.rs` | 线程编排 + 事件 | `AnalysisEvent` enum |
| `bridge/src/api.rs` | FRB 注解面 | 顶层 #[frb] 函数清单 |
| `bridge/src/bridge.rs` | 实现 + 转发 | `load_track_inner` → `forward_events` |
| `flutter/lib/engine/engine_controller.dart` | 60Hz 轮询 | `_tick` |
| `flutter/lib/widgets/manual_loop.dart` | 手动 loop 语义范例 | `_setOut`（raw 透传 + 引擎量化） |

## 5. 设计原则（全项目的「为什么」）

1. **实时线程零锁、零分配、零 IO**——所有数据流向都为此设计（总线无锁读、缓存预分配、事件块边界消费）
2. **解码永不在实时线程**——TrackCache 与独立分析线程双保险
3. **引擎是唯一事实来源，UI 只是视图**——UI 不保存播放状态，一切从快照读；量化/对齐等语义决策一律在引擎（P23 明确边界）
4. **以测试固守行为**——Rust 200 测试多数是「跑 N 块断言字节/数值」的门控测试（如 crossfader 居中 bitwise 恒等、sync 后 bpm 连续、loop 圈界对齐），改坏行为测试立刻红；UI 手感/听感由用户实机验证
5. **渐进里程碑**（P0–P23）——每步一个可播放、可测、可听的交付，回滚决策有记录（P17 落点量化否决即例）

## 6. 演进脉络速览

| 阶段 | 内容 | 留下什么设计 |
|---|---|---|
| P0–P2 | 骨架：core/总线 + 音频 + 简单播放 | 控制总线、TrackCache 预解码 |
| P3–P6 | keylock、beatgrid、sync、quantize | 引擎侧量化语义 |
| P7–P10 | 波形/overview、CUE、混音台、拍同步 | 渐进分析 + 播放头优先 |
| P11–P15 | loop 环重做、sync 速率锁、波形重构、beatjump 预卷 | loop 状态机定型 |
| P16–P18 | 精确跳距、nudge 互换、三新控件 | 跳距 = N×60/bpm |
| P19–P22 | transport 重构、ManualLoop 定界、FX、缓存进度 | 组件解耦、raw 透传 |
| P23 | 全曲预解码深化 + loop ring feed + 分析接口层 | 本次重构：process_engine 环喂入、tempo_segments、cache_filled |

每一步的详细论证（当时为什么这么选、否掉了什么）在 `实现方案.md`——它是 gitignored 的工作文档，是最完整的「为什么」档案。

## 7. 常见疑问

- **为什么 playhead 比声音「慢」？** keylock 管线固定延迟 ≈560 帧（11.7ms），显示已做补偿，但不同路径（环喂入/线性）延迟不同——P22-C 改动态延迟契约后不再硬编码
- **为什么 loop 起圈要 blend？** 环首与环尾音频不连续，硬切会产生咔嗒；首圈渐变（blend）+ 圈界回绕 blend 双保险
- **为什么跳转要 warm_start？** keylock 内部状态（相位/重采样器）需要预热，冷启动会产生 ~33ms 静音
- **为什么控制点用字符串路径？** MIDI 映射/脚本/记录仪都要按名字寻址；enum 会随着外设扩展反复改接口
- **为什么 UI 测试能不开引擎？** Actions 抽象把所有引擎调用接口化，测试注入假实现；真引擎留给 3 个集成测试（需真音频设备，逐个串行）
