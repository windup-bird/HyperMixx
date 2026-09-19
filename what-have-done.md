# What's been done (as of 8b00c3a + 本轮未提交修改)

## 背景回溯
- 8b00c3a 之后其他人(fuck-cheap-model)做的改动导致 main 输出爆音,已整体回退到 8b00c3a。
- 本轮:审查 mixer/fx pipeline → 修复审查发现的问题 → 完成 CLI(新 pipeline API + fx 命令)。

## Mixer / FX pipeline 审查结论(8b00c3a 基线)

**判定:mixer 路径本身是干净的,爆音来自被回退的后续 commit,不在本基线。**

核实过的正确设计:
- 命令在 producer 线程块边界分发,热路径零锁零分配;`cpal::Stream !Send` 由"mixer 在 producer 线程上构建"解决
- `Param` 原子 + 指数逼近,alpha>=1 snap、NaN 消毒 → 扫频不 zipper
- bypass→engage 边沿触发 `Fx::reset()`,无陈旧滤波器记忆咔哒声
- Biquad:RBJ 公式、原地重设计保留状态(连续扫频收敛)、退化输入钳制、发散自愈 + 计数
- Limiter:8 帧前瞻、软拐点值/斜率双连续、ceiling 派生不存储、拐点下 bit-exact、NaN 输入兜底
- Output:回调外预分配 scratch、partial push 永不阻塞、声道重映射安全钳制
- 配速:ring 4096 帧(~93ms)room-gated 生产 + 0.9×块时长 sleep,与硬件钟锁定

### 本轮修复(审查发现)
1. `Output::underruns` → `overruns`:原计数器在 `write()` ring 满写不进时递增,实为 producer 侧 overrun;注释也声称错误方向。已改名并如实注释(callback 侧 underrun 目前无计数,记录在 doc)。
2. 删除 `PREFILL_FRAMES`:声称"启动前预填"的死代码,实际从未预填(结构性谎言)。
3. 次要:`mixer/mod.rs` 非测试构建的 unused import、`Biquad::coeffs` 仅测试使用加 `#[cfg(test)]`(清零 warning)。

### 已知设计决定(非 bug,不修)
- Cue(耳机)总线无 limiter —— 安全级只属于 master,这是设计。
- master 的 safety limiter 不出现在 `fx list master` —— 它是拓扑不是槽位。

### 削波爆音诊断与修复(用户反馈"流程可用仍爆音,音量过大削波")

根因不在 fx 默认配置,而是拓扑+时序问题,共修四处:
1. **同设备双流(主因)**:main 与 headphones 都开在 `default_output_device()`,PipeWire 在
   graph 里直接相加——main 带 limiter(≤−1 dBFS)但 cue 总线无 limiter(cue_send 1.0 两 deck
   全发,电平可达 2.0),DAC 实收 ~2.9 → PipeWire 硬削波。修复:`Mixer::new` 检测同物理
   设备的多条流,第二条降级 sink 不路由(设备选择是后续工作);`OutputConfig.gain` 生效
   (该字段此前从未被应用)。
2. **无预填**:stream 启动即消费空 ring → 启动期静音间隙。修复:`Output::open` 预填
   半 ring 静音(~46ms,PREFILL_FRAMES 复活并真用)。
3. **limiter 块量化增益**:整块统一增益在块边界阶跃 → 瞬态咔哒。修复:改逐样本包络
   (attack 三时间常数跨 LOOKAHEAD,release 指数),前瞻 8→64 帧(1.45ms);stereo-linked。
   新增回归测试 `gain_moves_smoothly_across_block_boundaries`(旧实现会在该测试跳 ~0.5)。

## CLI(本轮完成)

- `AudioPipeline::start(simple_dj())` 新 API:config 进、channels 出(`command_tx()` / `response_rx()`),启动失败同步退出
- `analyse` 改走 `pipeline.deck_source(deck_id)` 往返查询(producer 线程独占 mixer 数据)
- printer 线程改 `std::thread::scope`(response channel 借自 pipeline,不能 move)
- 新增 `fx` 命令族:`add / remove / list / set / on / off / trigger / pad press|release / help`
  - chain 地址:`master` / `m` / `deck0` / `d0` / `0`
  - kind 在 CLI 侧先过 `FxKind::parse`(错误即时返回)
  - `fx help` 从 registry 生成 kind/参数清单,永不与引擎脱节
  - `FxAdded` / `FxListed` 响应渲染(slot 状态 + 参数值)
- **`--config <file>` / `--print-config`**:TOML 拓扑文件,详见下节

## TOML 拓扑配置(本轮新增)

`MixerConfig::from_toml_str` + CLI `--config <file>` / `--print-config`:
- 文件 schema 由 `MixerFile`/`MasterFile` 小镜像结构定义(`[[channel]]` 单数、`[master]` 表、`[[output]]`),
  与 Rust 类型各自保有适合自己的命名;`deny_unknown_fields` 拒绝拼错的键并报行号
- `reference_toml()` 输出参考拓扑(`--print-config`),测试钉死它解析回 `simple_dj()`
- `OutputConfig`:文件里只需 `name` + `role`,其余(`id`/`channels`/`gain`)有默认;`id` 按位置重编(mixer 内)
- limiter 从文件是 opt-in(内重参考拓扑开启;手写配置拿到它所要求的)
- 未知 FX 名仍是构造错误(在 `build_chains`),不在解析层

## 本轮发现并修复的引擎 bug

- `producer_loop` 的 `drain_queries` 只在收到命令后执行——空队列直接 break,
  启动后立即发出的查询(`channel_count`/`deck_source`)会链直到下一条命令才被应答(或 5s 超时)。
  修为 `TryRecvError::Empty` 分支也 drain。自定义 3 通道拓扑由此才真正可用(CLI deck 数从引擎查询)。

## 集成测试(本轮完成)

- `tests/common/mod.rs` 的 `session()` 改新 API:headless `simple_dj()`(outputs 清空)——测试不再占用声卡
- engine / beatlock / decode / tone_faithful 全部适配,无需改测试体

## 测试 & 验证状态(当前)
- `cargo build --workspace` 全绿,零 warning
- `cargo test --workspace`:162 lib + 全部集成测试通过(1 项 ignored:需要 test.mp3 的时长测试)
- `scripts/phase_probe.sh`:6/6 轮通过,err ≤ +1 帧(真实声卡播放路径验证)
- CLI 冒烟:默认 / `--print-config`→`--config` 闭环 / 自定义 3 通道拓扑(deck2 加载播放、fx list 正确)/
  坏 TOML 报错带行号 / 拼错参数拒启

## 未提交;下一步可做
- commit 本轮改动
- callback 侧 underrun 计数(需 producer 侧采样回调填充量,当前无)
- 设备选择(`[[output]]` 指定非 default 设备;当前同设备去重只修症状,真正解法是 device 字段)
- ARCHITECTURE.md 仍描述旧五层无 mixer/fx 结构,待补 mixer/fx/TOML 章节
