# 任务：实现 Hypermixx 音频引擎基础框架（第一版）

## 项目目标

构建一个 Rust workspace，包含两个 crate：
- `hypermixx-audio`：音频引擎库
- `hypermixx-cli`：命令行界面

实现单 Deck 音频播放，支持从本地音频文件解码到内存，非阻塞跳转（jump），并实时输出到声卡。**暂时不实现时间拉伸（pitchshift 透传）和混音器（mixer）**，但需预留接口以便后续扩展。

## 技术栈

- Rust 2021 edition
- `cpal` 0.15 用于音频输出
- `rtrb` 0.3 用于无锁环形缓冲
- `symphonia` 0.5 用于音频解码（支持 MP3、WAV、FLAC）
- `crossbeam-channel` 0.5 用于线程间通信

## 工作区结构

```
hypermixx/
├── Cargo.toml                # workspace 配置
├── crates/
│   ├── hypermixx-audio/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── source/
│   │       │   ├── mod.rs       # Source trait
│   │       │   ├── decoder.rs   # 解码器
│   │       │   └── pool.rs      # PCM 内存池
│   │       ├── flow/
│   │       │   ├── mod.rs
│   │       │   ├── flow.rs      # Flow 播放单元
│   │       │   └── pitchshift.rs # 时间拉伸引擎（暂为透传）
│   │       ├── deck/
│   │       │   ├── mod.rs
│   │       │   ├── deck.rs      # Deck 核心能力
│   │       │   └── timeshift.rs # 跳转协调器
│   │       ├── ringbuf.rs      # 环形缓冲封装
│   │       └── pipeline.rs     # 生产者线程 + 音频输出
│   └── hypermixx-cli/
│       ├── Cargo.toml
│       └── src/
│           └── main.rs
```

## 模块职责与关键接口

### 1. `source/mod.rs` — Source trait

```rust
pub trait Source: Send + Sync {
    /// 从 start_frame 开始读取 PCM 数据到 output（交错立体声 f32）
    /// 返回实际读取的帧数
    fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize;
    /// 音源总帧数
    fn total_frames(&self) -> u64;
}
```

### 2. `source/decoder.rs` — 解码器

- 函数：`pub fn decode_file(path: &str) -> Result<DecodedAudio, Box<dyn Error>>`
- 返回结构：
  ```rust
  pub struct DecodedAudio {
      pub pcm: Vec<f32>,       // 交错立体声，48kHz
      pub total_frames: u64,
      pub sample_rate: u32,    // 应为 48000
      pub channels: usize,     // 应为 2
  }
  ```
- 使用 `symphonia` 解码，可能需要重采样到 48kHz（可暂时简单处理或假设输入为 48kHz，但需注明）。
- 解码耗时操作，在后台线程调用。

### 3. `source/pool.rs` — PCM 内存池

```rust
pub struct PcmPool {
    pcm: Arc<Vec<f32>>,
    total_frames: u64,
}

impl PcmPool {
    /// 从 DecodedAudio 创建（可能涉及重采样处理）
    pub fn from_decoded(decoded: DecodedAudio) -> Self;
}

impl Source for PcmPool {
    fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize;
    fn total_frames(&self) -> u64;
}
```

### 4. `flow/pitchshift.rs` — 时间拉伸引擎（透传版）

```rust
pub struct PitchShiftEngine {
    source: Arc<dyn Source>,
    current_input_frame: u64,
    ratio: f32,               // 暂不使用
}

impl PitchShiftEngine {
    pub fn new(source: Arc<dyn Source>, ratio: f32) -> Self;
    /// 处理一个块（BLOCK_SIZE 帧），从 source 读取原始数据并复制到 output
    pub fn process_block(&mut self, output: &mut [f32]) -> usize;
    /// 预热到目标帧（此版本直接设置位置，无需真实预热）
    pub fn prepare_jump(&mut self, target_frame: u64);
    /// 轻量重置到目标帧（循环时使用，此版本同 prepare_jump）
    pub fn reset_to(&mut self, target_frame: u64);
    /// 当前输入帧位置
    pub fn current_frame(&self) -> u64;
    /// 设置播放速率（暂存，不改变行为）
    pub fn set_ratio(&mut self, ratio: f32);
}
```

### 5. `flow/flow.rs` — Flow 播放单元

```rust
pub enum FlowState {
    Preparing,
    Ready,
    Active,
    Retired,
}

pub struct Flow {
    pub id: u64,
    pub state: FlowState,
    pub start_frame: u64,
    pub end_frame: Option<u64>,   // None 表示直到末尾
    pitchshift: PitchShiftEngine,
    ready_tx: Option<Sender<u64>>,  // 预热完成后发送自己的 id
}

impl Flow {
    pub fn new(id: u64, source: Arc<dyn Source>, start_frame: u64, end_frame: Option<u64>, ready_tx: Sender<u64>) -> Self;
    pub fn process_block(&mut self, output: &mut [f32]) -> usize;
    pub fn current_frame(&self) -> u64;
    pub fn reached_end(&self) -> bool;
    pub fn mark_ready(&mut self);  // 设置状态为 Ready 并发送 id
    pub fn activate(&mut self);
    pub fn retire(&mut self);
}
```

### 6. `deck/timeshift.rs` — 跳转协调器

```rust
pub struct TimeShift {
    prepare_tx: Sender<Flow>,      // 发送 Flow 到后台预热线程
    ready_rx: Receiver<u64>,       // 接收预热完成的 Flow id
}

impl TimeShift {
    pub fn new() -> Self;
    pub fn submit_prepare(&self, flow: Flow);
    pub fn poll_ready(&self) -> Option<u64>;  // 非阻塞
}
```

- 初始化时启动一个后台预热线程，从 `prepare_tx` 接收 Flow，立即调用 `flow.pitchshift.prepare_jump(flow.start_frame)`（透传版几乎无耗时），然后调用 `flow.mark_ready()`，将 id 发送到 `ready_rx`。

### 7. `deck/deck.rs` — Deck 核心能力

```rust
pub struct Deck {
    pool: Arc<dyn Source>,
    flows: Vec<Flow>,
    active_index: usize,
    timeshift: TimeShift,
    next_flow_id: u64,
    playing: AtomicBool,
}

impl Deck {
    pub fn new(pool: Arc<dyn Source>) -> Self;
    pub fn play(&self);
    pub fn pause(&self);
    pub fn is_playing(&self) -> bool;
    /// 非阻塞跳转：创建新 Flow 并提交预热
    pub fn jump(&mut self, target_frame: u64);
    /// 生产者线程每块调用
    pub fn process_block(&mut self, output: &mut [f32]) -> usize;
    /// 当前播放位置
    pub fn current_frame(&self) -> u64;
    /// 内部：检查就绪流并切换
    fn poll_ready_flows(&mut self);
    /// 内部：执行切换
    fn switch_to(&mut self, flow_id: u64);
}
```

- `jump` 创建新 Flow（end_frame=None），设置 `ready_tx` 为 TimeShift 的发送通道，然后调用 `timeshift.submit_prepare(flow)`。
- `process_block` 中先调用 `poll_ready_flows()`，检查 `timeshift.poll_ready()`，若存在则切换，然后处理活跃 Flow。如果 `playing` 为 false，输出静音。

### 8. `ringbuf.rs` — 环形缓冲封装

```rust
pub struct AudioRingBuffer {
    producer: rtrb::Producer<f32>,
    consumer: rtrb::Consumer<f32>,
}

impl AudioRingBuffer {
    pub fn new(capacity_frames: usize) -> Self;
    pub fn push(&mut self, data: &[f32]) -> usize;
    pub fn pop(&mut self, output: &mut [f32]) -> usize;
    pub fn available(&self) -> usize;
    pub fn consumer_clone(&self) -> rtrb::Consumer<f32>;
}
```

- 容量建议：4096 帧（即 4096 * 2 个 f32）。

### 9. `pipeline.rs` — 生产者线程与音频输出

```rust
pub struct AudioPipeline {
    deck: Arc<Mutex<Deck>>,               // 仅在生产者线程访问
    output_ring: Arc<Mutex<AudioRingBuffer>>,
    _stream: cpal::Stream,                // 保持音频流存活
}

impl AudioPipeline {
    pub fn start(command_rx: Receiver<Command>, response_tx: Sender<CommandResponse>) -> Self;
    fn producer_loop(...);
    fn audio_callback(...);
}
```

- 生产者线程循环：
  - 非阻塞处理 `command_rx` 中的命令（Load/Play/Pause/Jump/GetState 等）。
  - 如果 Deck 存在且 playing，调用 `deck.process_block(block_buffer)`，将结果写入输出环形缓冲。
  - 如果 Deck 不存在或 playing=false，输出静音块或跳过写入（但环形缓冲需要保持数据流，建议持续写入静音块以防止欠载）。
- 音频回调：
  - 从环形缓冲读取数据填充到 `cpal` 提供的缓冲区。
  - 如果数据不足，填充 0.0（静音）。
- 启动时预填充输出环形缓冲：在开始音频流前，先让生产者线程运行一段时间（或手动填充 2048 帧静音），确保音频回调启动时不会立即欠载。

### 10. CLI (`hypermixx-cli/src/main.rs`)

- 创建 `crossbeam_channel` 用于命令（`Command`）和响应（`CommandResponse`）。
- 定义 `Command` 枚举（可放在 `hypermixx-audio` 中或单独 crate）：
  ```rust
  pub enum Command {
      Load { path: String },
      Play,
      Pause,
      Jump { target_frame: u64 },
      GetState,
  }
  pub enum CommandResponse {
      Loaded { total_frames: u64 },
      State { current_frame: u64, playing: bool },
      Ok,
      Error(String),
  }
  ```
- 启动 `AudioPipeline::start()`，将命令发送器和响应接收器传入。
- 启动一个命令行循环（stdin），解析用户输入并发送命令，打印响应。
- 支持命令：`load <path>`、`play`、`pause`、`jump <frame>`、`state`、`quit`。
- `load` 命令在后台线程执行解码，完成后发送 `Loaded` 响应，此时 Deck 被创建。

## 实现步骤

1. 初始化 workspace 和两个 crate。
2. 定义 `Source` trait 和 `PcmPool`。
3. 实现 `Decoder`（用 symphonia 解码，处理采样率转换和声道转换，输出 48kHz 立体声 f32）。
4. 实现 `PitchShiftEngine` 透传版。
5. 实现 `Flow`。
6. 实现 `TimeShift` 及其后台预热线程（直接完成预热）。
7. 实现 `Deck`。
8. 实现 `AudioRingBuffer`。
9. 实现 `Pipeline`，包含生产者线程和 cpal 输出。
10. 实现 CLI。
11. 编译测试，调整问题。

## 验收标准

- `cargo build` 无错误。
- `cargo run -p hypermixx-cli` 启动后可以输入命令。
- `load <音频文件路径>` 成功加载并返回总帧数。
- `play` 开始播放，能听到声音。
- `jump <目标帧>` 执行跳转，无爆音或明显卡顿，播放位置改变。
- `pause` 暂停，`play` 恢复。
- `state` 显示当前播放位置。
- 程序退出无错误。

## 注意事项

- **音频回调线程绝对不能阻塞、分配内存或使用锁**，只能从环形缓冲读取数据。
- **所有共享状态使用原子类型或无锁结构**（如 `AtomicBool`、`rtrb`、`crossbeam_channel`）。
- 命令处理在生产者线程中使用 `try_recv` 非阻塞循环。
- 解码在独立线程中执行，避免阻塞命令响应。
- 预填充输出环形缓冲（如 2048 帧）以避免启动欠载。
- 当 Deck 尚未加载或 paused 时，生产者线程应持续向环形缓冲写入静音块，保持音频回调有数据可读，避免欠载。
- 代码必须处理错误情况（如文件不存在、解码失败等），并返回错误响应。
- 常量定义：
  - `SAMPLE_RATE: u32 = 48_000`
  - `CHANNELS: usize = 2`
  - `BLOCK_SIZE: usize = 256`（每块帧数）
  - `OUTPUT_RING_CAPACITY: usize = 4096`（帧）
  - `PREFILL_FRAMES: usize = 2048`

请按照以上要求编写代码，确保架构清晰、可扩展，为后续加入时间拉伸、混音器等功能奠定基础。
