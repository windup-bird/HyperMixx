# Hypermixx 架构

Rust workspace,七个 crate 单向分层:**core(类型) ← media(PCM) ← audio(引擎)**;
library(分析)与 audio 零交叉依赖,只被 cli 调用;stems(离线分离)同理,只是多一个 ML 依赖
且可以关掉。引擎域 **44.1 kHz 立体声**(与目标设备 ALSA default 的原生时钟一致,cpal 回调直通,
零转换)。

```
cli ──→ core / media / audio / library / stems / midi
audio ──→ core, media          (+ signalsmith-stretch, vendored C++; serde + toml)
library ──→ core, media        (+ stratum-dsp, git 依赖 rev 锁定)
stems ───→ core, media         (+ charon-audio/ort, `onnx` 默认开, `cuda` 可选)
midi ───→ core                 (+ midir, serde + toml)
media ──→ core                 (+ symphonia)
core ──→ serde
```

```
crates/
├── hypermixx-core/       # 协议与类型(仅依赖 serde)
│   └── src/
│       ├── action.rs    # 唯一的 action 语法表(名字就是会话命令行,CLI/guide/文档共用)
│       ├── beatgrid.rs   # BeatGrid:绝对帧拍网格 + 相位/拍宽查询
│       ├── key.rs        # Key / KeyMode / KeyFormat(传统 + Camelot)
│       ├── analysis.rs   # TrackAnalysis { beatgrid, key, bpm }
│       ├── deck.rs       # DeckId = u8 / DeckState 快照
│       ├── command.rs    # Command / CommandResponse / Backend / FxChainId / FxSlotRef
│       ├── live.rs       # LiveConfig:引擎率 / ring / stem 默认值(TOML 由前端解析)
│       ├── output.rs     # OutputRole / OutputState / OutputStatus(设备绑定的报告形状)
│       └── source.rs     # Source trait + Shared = Arc<dyn Source>
│
├── hypermixx-media/      # PCM:解码 + 内存池(core + symphonia)
│   └── src/
│       ├── chunked.rs    # ChunkedStreams:可增长的 n 路流(水位外回落 + 前沿淡化),与 stem 无关
│       ├── decoder.rs    # decode_file:symphonia → 44.1kHz 立体声 f32
│       └── pool.rs       # PcmPool:不可变 Arc 数据的 Source 实现
│
├── hypermixx-audio/      # 实时引擎(core + media + signalsmith-stretch + serde/toml)
│   ├── src/
│   │   ├── ringbuf.rs    # rtrb SPSC 封装 + 自由函数
│   │   ├── pipeline.rs   # AudioPipeline:producer 线程 + 命令/查询通道
│   │   ├── deck/
│   │   │   ├── deck.rs      # Deck:流状态机 + cued_from 跳转补偿 + pull_into + apply_sync
│   │   │   ├── jump.rs      # Seek{Frames,Beats,Beat,Quantized} + phase_preserving
│   │   │   ├── loop_.rs     # LoopRange/原子 cell/LoopSource(virtual→actual 映射读) + 量化
│   │   │   └── sync.rs      # Playhead(tempo+nudgerate)/PhaseAlign/Pll/Nudge/SyncCtx
│   │   ├── flow/
│   │   │   ├── playing.rs    # Playing:一个独立播放声部(引擎+时钟+loop cell)
│   │   │   └── pitchshift.rs # PitchShiftEngine:Signalsmith / Tape 后端
│   │   ├── sync.rs          # SyncGroup:谁领谁跟 + 每块预扫(编排,不进 mixer)
│   │   ├── mixer/         # 混音拓扑(见下)
│   │   └── fx/            # 效果子系统(见下)
│   └── tests/            # engine / beatlock / loop / sync / decode / tone_faithful
│
├── hypermixx-stems/      # 离线 stem 分离(core + media + charon/ort, feature `onnx`)
│   ├── src/chunked.rs    # 流式驱动:StemBlockSink + 缓存按块追加/续跑 + InputScale(整轨归一化)
│   └── src/
│       ├── lib.rs          # StemSeparator trait / Cancelled / read_source / separate_cached
│       ├── mock.rs         # MockSeparator:无模型,CI 与开发用(两种精确可加的切法)
│       ├── model.rs        # 模型表 + 镜像下载 + SHA-256 校验(不信任后端的下载器)
│       ├── cache.rs        # 内容寻址缓存(./.cache/stems/<key>/);模型在 ./.model(`HYPERMIXX_CACHE`/`HYPERMIXX_MODELS` 可覆盖)
│       ├── onnx.rs         # CharonSeparator:进度/取消桥接 + 帧对齐契约检查 + EP 选择
│       └── onnx_options.rs # Provider(cpu|cuda)/SeparateOptions:改音频的旋钮,也就是缓存 key
│
│   # charon-audio(CUDA EP)不在此树内:它是 fork windup-bird/charon-audio-cuda,
│   # 由根 Cargo.toml 的 [patch.crates-io] 接管;见该 fork 的 FORK.md
│
├── hypermixx-library/    # 分析与曲库(core + media + stratum-dsp)
│   └── src/
│       ├── beat_spec.rs     # BeatSpec/Segment:可编辑网格真源
│       ├── grid_compiler.rs # GridCompiler:前填/等距/接缝去重/尾外推 → BeatGrid
│       ├── track.rs         # TrackInfo:持 BeatSpec,惰性缓存编译结果
│       ├── waveform.rs      # 立体声三频段 u8 峰值概览（2:1 mipmap）
│       └── analyser/
│           ├── mod.rs         # analyze(source) = backend → refine → compile
│           ├── stratum.rs     # stratum-dsp 适配(兜 panic,关静音裁剪)
│           ├── timestretch.rs # 占位:上游未暴露离线分析,返回 Unsupported
│           └── refine.rs      # fit_rigid:中位数周期 + 最小二乘 + 倍频归位
│
├── hypermixx-midi/       # MIDI 输入映射(core + midir + serde/toml)
│   └── src/
│       ├── msg.rs        # Decoder:字节流 → Event(丢 clock/sysex,归一 vel0)
│       ├── map.rs        # TOML schema + action 注册表(manifest)+ 校验/序列化
│       ├── translate.rs  # translate():Event → Command(pickup/相对编码器/latest-wins)
│       ├── guide.rs      # guide:learn 状态机(纯逻辑,无 I/O/渲染)
│       └── ports.rs      # midir 端口枚举/打开 → Sender<Received>
│
└── hypermixx-cli/        # 前端(二进制,依赖全部五层)
    ├── src/main.rs       # 启动参数 / 拓扑+live+profile 的读取 / 前端装配
    ├── src/command.rs    # 行解析 / fx 命令族 / 后台 decode+analyse
    ├── src/profile.rs    # profile:指路文件(mixer/live/midi),路径相对所在文件夹,preference 预留
    └── src/session.rs    # 一次会话交给前端的全部东西(引擎 + 用户选择)
```

### 外部依赖(git,rev 锁定)

| crate | 来源 | 锁定 |
|---|---|---|
| `stratum-dsp` | `github.com/HLLMR/stratum-dsp`(第三方库) | `rev = 758e0b6` |
| `signalsmith-stretch` | `vendor/signalsmith-stretch`（上游 1.3.2 + `outputSeek` wrapper） | 本地路径 |

`signalsmith-stretch` 连同 C++ 头文件一起 vendor，不依赖 git submodule。
此外 `midir` 0.11 为 crates.io 依赖(MIDI 输入);其 ALSA 后端复用 cpal 已引入的
`alsa`/`alsa-sys`,不新增 C 依赖。

---

## hypermixx-core

只放类型与协议。`Source` 定义在这里是 `Command::Load` 能携带 `Arc<dyn Source>` 的前提
(若在 media,core→media 成环);所有 crate 通过 `use hypermixx_core::…` 共享。

### `BeatGrid`
绝对帧位置 `Vec<u64>`(严格递增),BPM 永远派生、不存储。
- `from_constant_bpm(bpm, first, total, sr)` — 恒速网格,无累积误差
- `from_frames(beats, sr)` / `empty(sr)` / `is_empty()`
- `frame_at_beat(beat)` — 存储范围内直接索引,越界按末拍间隔外推
- `floor_beat(frame)` / `phase(frame)` / `beat_width(beat)` / `average_bpm()`
- `bpm_at_beat(beat)` / `bpm_at_frame(frame)` — 逐拍派生的**瞬时 BPM**,sync 用它而不是
  全轨均值(带段落变速的网格必须知道“此刻这里多快”)

### `Command` / `CommandResponse`
- `Load { deck_id, source, analysis }` — source 已解码,analysis 可选(常网格快路径)
- `Play` / `Pause` / `TogglePlay` / `Cue { deck_id, op }` / `Jump { target_frame }` / `BeatJump { beats }`;
  `CueOp = Play | Back | Set | Smart`(`Smart` 由 deck 自己的 `is_playing()` 决定,前端不猜)
- `SetTempo` / `SetTempoFader` / `SetTempoRange` / `SetKeylock { mode: KeylockMode }` / `SetKey { semitones }` / `SetAnalysis` —
  分析结果从这里进入 deck,**没有 `Analyse` 命令**;`KeylockMode = Off | On`(默认 On),
  STFT 窗口是 live.toml 的 `[keylock]` 设置(见 `KeylockSettings`)
  `SetKey` 是占位(streaming 引擎无变调轴)。**推子是控制器输入**:`SetTempoFader{position}`
  由 deck 按 `1 + position×tempo_range` 算 rate,再走 `SyncGroup::set_tempo` 的三分支
- sync 族:`Sync { deck_id, op }`,op = `Tempo` / `Phase{mode,t}` / `TempoLock` /
  `PhaseLock{mode,t}` / `SetLeader` / `Unlock`;`PhaseMode = Instant | Linear | Pid`。
  **`phase` 包含 `tempo`,`phaselock` 包含 `tempolock`**;`SetLeader` 不带参数(target-first
  语法下 `deck0 sync set-leader` 即“deck0 是 leader”)
- `Nudge { deck_id, op }`,`op = Start{delta, seconds} | Stop` — 临时速率弯折,
  只动 `nudgerate`,结束时 `tempo` 原封不动(CLI 秒数必填,MIDI `seconds=None` 为按住)
- `DeckState` 除 transport 外还报 `bpm` / `bpm_at_frame` / `tempo` / `tempo_fader` / `tempo_range` / `nudgerate` / `playing_rate` / `lock` / `align` /
  `nudge` / `sync_leader` / `sync_mode` / `group_bpm` / `cue_frame` / `keylock` / `key_shift`
  —— TUI 徽标与 `sync_phase.sh` 的判据都读它(deck 信息行的 `keylock on|off` 直接读 `DeckState`);
  前端的“当前 BPM” = `bpm_at_frame × tempo`(`response::bpm_label`,不含 nudgerate)
- `GetState` / `GetAllStates`(同块原子快照)/ `Quit`
- FX 族:`AddFx { chain, kind }` / `RemoveFx { chain, index }` / `SetFxEnabled` /
  `SetFxParam { slot, name, value }` / `FxTrigger` / `PadPress` / `PadRelease` / `ListFx`;
  槽位寻址 `FxChainId::{Master, Deck(u8)}` + `FxSlotRef { chain, index }`
- `Backend { Auto, Stratum, Timestretch }` — 分析后端选择

---

## hypermixx-media

- `decode_file(path)` — symphonia 解码 → 重采样到引擎率(44.1 kHz)→ 立体声交错 f32。
  非 44.1k 素材在加载时一次性转换,引擎内不再有采样率概念。
- `PcmPool` — `Arc<Vec<f32>>` 的 `Source` 实现,克隆即 Arc bump;deck 与分析器共享同一条数据。

---

## hypermixx-audio

### `pipeline.rs`
拓扑:`CLI → [producer 线程: Mixer::process] → 各输出 ring → [cpal 回调] → 声卡`

- `AudioPipeline::start(MixerConfig)` — mixer 在 producer 线程上**构建并持有**
  (`cpal::Stream` 是 `!Send`,不能跨线程交接;坏配置同步回报,启动失败即干净退出)
- 命令(`Command`)与查询(`deck_source` / `channel_count`)各一条 crossbeam 通道;
  查询在命令队列空时也 drain(否则启动后立即发出的查询会饿死)
- 热路径:`SyncGroup::prepare → make_ctx → Mixer::process`,零锁零分配;回调只读 ring。
  `prepare` 在任何 deck 渲染**之前**取样全 deck 位置并把各自的 `SyncCtx` 塞回去——
  follower 比的必须是 leader **同一块**的位置,不是上一块的
- 配速双源:ring 余量(设备钟)优先,无设备时挂钟;sleep 取 0.9× 块时长保持余量
  (所以无设备时比挂钟快 11%,有设备时由 ring 反压纠正回来)
- `Drop` **发 `Quit`** 而不是只关 channel:调用方可能还握着 `command_tx` 的 clone,
  等它断开才能退出会死锁(测试 panic 时就是这个形状)

### `sync/` — 拍同步编排(**不是 mixer 的事**)

mixer 只负责把 deck 变成采样;谁跟谁、共享 BPM 是什么,是**关于 deck 的编辑决策**。
所以独立成模块,由 producer 与 mixer **并列持有**,经 mixer 已有的 `deck()`/`deck_mut()`
访问——`Mixer::process` 里没有一行 tempo / 相位代码。

- `SyncGroup` — `leader` / `follower` / `mode`(`Free|Tempolock|Phaselock`)/ `group_bpm`
  + 每块位置快照
  - `prepare(&mut Mixer)` — 每块渲染前:取样全 deck(phase / `bpm_at_frame` / `tempo`)、
    phaselock 下用 `leader.tempo × bpm` 重算 `group_bpm`、给每个 deck 塞 `SyncCtx`(`Copy`)
  - `handle_sync` / `set_tempo` / `nudge` — 命令入口。**fader 三分支**:
    phaselock+follower → 忽略(单向)/ tempolock → 写 `group_bpm`(双向)/ 其余 → 写自己 `tempo`
  - **单向规则**:`track()` 拒绝反向——两 deck 互相同步拒绝第二条;显式 leader 拒绝自同步
  - **锁下不再接受一次性匹配**:`sync tempo` 在 `Tempolock`/`Phaselock` 下直接拒绝——它只会写
    `tempo`,而下一块就会被 `group_bpm` 重算盖回,回 `Ok` 却什么也没发生,比报错更糟。
    `sync phase` 在锁下则**跳过对拍那一步、只装 align**(组已经管着 tempo,方向照样校验),
    所以“锁定期间改相位模式”是合法操作。对照:**phaselock 下 follower 的 fader 是故意忽略**的
    ——那是连续控制,演出中途不能抛错
  - **`tempo` 与 `nudgerate` 永不混**:`group_bpm` 只由 `tempo` 派生,所以 nudge 与相位修正
    是个人行为,不会反馈进组速度再弹回来
  - 领袖必须有网格,否则拒绝(拿 0 BPM 当 leader 比静音还糟)
  - **`sync unlock` 解的是整组**:命令只带一个 `deck_id`,但 lock 本来就是**两个 deck** 的事——
    不清对侧,就会留下一个还在向一个已不存在的组取速度的 deck。所以 `[deck_id, leader, follower]`
    三者的 playhead 一起清(`lock`/`align`/`nudgerate` 归零),`mode` 回 `Free`、`follower` 置空、
    `group_bpm = 0`;**`tempo` 一个都不动,`leader` 也保留**(`set-leader` 是关于谁领的声明,
    不是锁的一部分)

### `mixer/` — 拓扑与混音

**设计原则:顺序是设计参数,不是计算结果。** 没有路由图、没有拓扑排序——读者从信号源
就能预测它经过了什么。配置决定"链上放什么",代码决定"信号怎么走"。

固定信号路径(`Channel::process`):

```
(逐 stream 循环:一个 deck 有 1 条,装了 stems 就有 4 条)
    deck.render_stream(i)
    → source_fader[i] 逐流**门**:stem 的 mute/solo 决策,恰好 1.0 或 0.0(GATE_TAU 平滑)
    → [cue tap]       PostSourceFader = 门之后、插入之前(听得到"裸声")
    → flow_fx[i]      逐流插入(换流即重置滤波记忆;**每 stream 一条,状态独立**)
    → flow_fader[i]   逐流电平(只衰减,行程顶端 0 dB)
    → Σ 到 channel bus
  → [cue tap]         PostFlowFader  = Σ 各路 flow_fader 之后
  → deck_fx           逐 deck 音色(跨跳转持久,整 deck 共用)
  → deck_fader        逐 deck 电平(只衰减)
  → [cue tap]         PostDeckFader
  → gain              channel trim:0 dB .. +6 dB(唯一能 boost 的一级)
  → 求和
```

共享 crossfader 的按侧增益坐在三颗 fader 之一的位置(`[crossfader] place = source|flow|deck`,
缺省 `deck` = 历史位置),**恒在与其同名 cue tap 之后**,所以分叉点与它是同一点时 cue 不受
crossfader 影响;分叉在它之后则跟着它(把 crossfader 放早的代价,文档写明)。

单 stream、且 crossfader 在 `deck` 位置时,上面这段与历史路径**逐位相同**。

`MasterBus::render`:`sum → master_fx → master_fader → limiter(安全级)`。
`CueBus`:`sum → cue_fader → blend_with(master.sum)` —— 耳机是 cue 与 master 的混合,
`[cue] blend` `-1`(缺省,= 只听 cue)/`0`/`+1`,三条曲线与 crossfader 共用。

四样东西**永远不可配置**:

1. limiter 是 `MasterBus` 的字段而非槽位——配置忘写 `"limiter"` 也不可能把全尺度音频
   送到 DAC(且不走链跳过优化,engaged 即执行)
2. cue tap 的三个位置是枚举,不是自由路由
3. **fader 只衰减**:`fader_amp` 的上半段恒为 1.0;headroom 只在 `gain`(`trim_amp`,+6 dB 封顶)
4. 阶段顺序本身

其余由 `MixerConfig` 纯数据描述(`config.rs`):每通道的 flow_fx/**stem_fx**/deck_fx 名单
(`Vec<String>`,经 `FxKind` 注册表解析,未知名 → `MixerError::UnknownFx` 构造失败),
`[crossfader]`(place/curve/cut_width/start)、`[cue]`(fader/blend/curve/cut_width)、
以及 fader 起始位、cue_send/cue_tap/side、master fx/limiter/fader、输出列表
(name/role(`master`|`cue`)/channels/gain)。
`flow_fx` 是**模板**:每条 stream 各建一条独立的链(4 stems = 4 条,各自状态),`[channel.stem_fx]`
按 stem 覆写模板 —— 所以"只给人声加 filter"可以写在配置里,不必只靠运行时命令。

- `Bus` — 面内(L/R 分离)块单位,构造时分配,热路径零分配;效果器逐面迭代省去步长乘法
- `Channel` — 拥有自己的 deck、**每 stream 一条 flow 链**(`Vec<FxChain>`,从 config 的
  `flow_fx` 名单当模板重建)、一条共享 deck 链、每 stream 一个 fader;**没有合并索引空间**
  (`FxChainId::Deck` 只指 deck 链,`FxChainId::Stem{deck,stem}` 指某条 stem 的链 —— 一条链
  一个 stream 之后,"deck 的 slot 2" 可以指四个不同的效果)。逐 stem 的 level/mute/solo 存成
  **三件独立事实**,由 `refresh_stem_faders` 一处合成成**两颗门 + 四颗 fader**:solo 能压过 mute、
  取消静音能恢复原电平(而不是回到 unity),而被静音的 stem 交给自己的插入链的是真静音
- `Output`(`output.rs`,crate 里唯一出现 cpal 类型的地方)— 每输出独立 ring + stream;
  回调外预分配 scratch、partial push 永不阻塞、声道重映射安全钳制(单声道取和);
  `gain` 逐目的地 trim;**预填半 ring 静音**(~46ms)避免启动竞速;producer 侧 overrun
  有计数(callback 侧 underrun 与真实静音不可区分,故不数)。设备按 `device` 模式解析
  (**名字子串**或序号,缺省 = 系统默认),状态四态 `live | sink | missing | disabled` ——
  "设备没插"和"设备不接受引擎率"都是**状态而不是失败**:引擎照跑、`output refresh` 重试
  (`Mixer::refresh_outputs` 重新跑一遍 `build_outputs`,先 drop 旧流再开新流,免得两条流抢同一设备)
- **同一物理设备只保留一条流**:master+cue 都落在 default device 时,PipeWire 会在
  graph 里把两条流直接相加——main 有 limiter(≤−1 dBFS)但 cue 总线没有(cue_send 1.0
  时电平可达 2.0),DAC 实收 ~2.9 → 硬削波。Mixer 检测设备名重复,第二条降级 sink 不路由
  (设备选择 `[[output]] device` 字段是后续工作)
- `simple_dj()` — 参考拓扑:2 通道(各 eq+filter,cue_send 1.0,tap PostDeckFader)+
  master limiter + master/cue 输出;`reference_toml()` 是它的 TOML 文本 —— 直接
  `include_str!("profile/mixer.toml")`,所以文件、`--print-config` 与内置构造只有一个真源
  (测试钉死解析回 `simple_dj()`)

**热加载**:`Command::MixerReload { text }`(文本而非路径 —— 文件 IO 属于前端)在 producer 线程上
`Mixer::reload(&text)`:先解析+`build_chains` 全量校验,通过后才就地换掉 channels/chains/faders/
master/cue 并重建输出。**配置值取"默认 ← 文件"(未定义即默认),不是补丁**;保留的只有不属于配置的东西
—— 每个 channel 的 deck 与走带(`Channel::take_deck`)、计量计数。文件里少掉的 channel 连同曲目一起丢,
多出来的建空 deck。任一错误整份拒绝,旧链照常工作到验证通过为止。

**TOML 配置**(`MixerConfig::from_toml_str`):文件 schema 由 `MixerFile`/`MasterFile`
镜像结构定义(`deny_unknown_fields`,坏键报行号),Rust 类型与文件格式各自保有适合的
命名。每 `[[channel]]` 一 deck,`[master]` 一表,`[[output]]` 任意个;除 `side`/`name`/
`role` 外全部有默认。解析与构造分层:未知 FX 名在 `build_chains`(注册表查询)而非解析层。

### `fx/` — 效果子系统

- `Fx` trait — 只见 `Bus` + `FxContext`(采样率/块长/拍网格),六个方法五个有默认体;
  实现须 `Send`、`process` 内不分配不阻塞不加锁
- `Param` — 参数跨线程的唯一通道:双原子(u32 位转换)+ 指数逼近,块级(`next_block`)
  或逐样本(`next_sample`)步进;`alpha≥1` snap、NaN 消毒、settled 收尾——扫频不
  zipper、fader 移动不咔哒
- `FxSlot` — 拥有效果实例(`Box<dyn Fx>`);bypass→engage 边沿触发 `Fx::reset()`
  (不带陈旧滤波器记忆复活的咔哒声);pad 按住即 engaged,松开恢复最近意图
- `FxChain` — `Vec<FxSlot>` 有序表,槽位增删在 producer 线程块边界(move,无锁热交换)
- `dsp.rs` — RBJ biquad(LPF/HPF/BPF/bell/shelf):bell 用 `10^(dB/20)`、shelf 用
  `10^(dB/40)`(各自正确的 A 幂次);原地重设计保留状态(连续扫频收敛)、退化输入
  钳制、发散自愈 + 计数;`fader_amp`(只衰减,−1 恰好静音、0 dB 封顶)与 `trim_amp`
  (下半个行程同律,上半段到 +6 dB)
- `sample/` 四个内置效果:
  - `Eq` — 三段 ISO:low shelf 320Hz / bell 1.2kHz / high shelf 3.6kHz,±26 dB 线性 dB
    律;系数仅在控制移动时重算,平坦时整段跳过
  - `Filter` — 共振扫频:LP/HP(Q 律)/ BP(恒峰值增益,构造性安全);cutoff 指数映射
    20Hz..18kHz,模式热切换
  - `Gain` / `Fader` — 线性增益(mixer 自己的 fader 也建在这上)
  - `Limiter` — 安全级:**64 帧前瞻**(1.45ms,位于混音后不影响 deck 相位)+ **逐样本
    增益包络**(attack 三时间常数跨前瞻窗,块边界无增益阶跃=无瞬态咔哒;立体声联动);
    软拐点值/斜率双连续、拐点下 bit-exact;ceiling 派生不存储(backoff 机制),纳米输入
    兜底;任何输入不超 ceiling
- `registry.rs` — `FxKind` 封闭枚举(eq/filter/gain/limiter)+ 别名解析;加效果 =
  一个 variant + 一个 `build` arm + 一行 `ALL`,无插件面

### `deck/`
- `Deck` — `sources: Vec<Shared>`(1 条 = 普通曲目,4 条 = stems)、单活跃 flow;
  `set_sources()` 在当前 virtual 位置准备一个新 `Playing`，下一块硬切（带 loop range），
  所以"播放中装 stems"是无缝的;`begin_block`/`render_streams`/`end_block` 三相位拆分,让 mixer
  能在同一块内先让换源落地、再按 `stream_count()` 备好逐 stream 的链;`jump()` 记 `cued_from`(**virtual 时钟**),`switch_to()` 把新流
  落点前移预热期间已播放的帧量,保证跨 deck 相位锁定(`phase_probe.sh` 的 `err` 恒 +0);
  `pull_into(&mut Bus, &FxContext)` 是 `process_block` 的面内薄包装
- **两个时钟**:`virtual_frame()` = 引擎播放头,只增不减、loop 不碰(slip);
  `current_frame()` = `map(virtual)` = 听到的位置(波形/state/beatjump 都读它)
- **Loop 状态机**:deck 持有 `playing` + 可选 `shadow`。普通 loop 的 `shadow` 提前按
  provisional range 喂好，`loop out` 硬切；beat loop 同样准备后硬切。slip 模式不丢弃
  `playing`：`playing` 继续 slip 时钟（静音），`shadow` 是听到的 loop/roll/vinyl，退出只丢
  `shadow`。`loop exit` 从当前可听位置硬切；循环中编辑只改 range，不移动时钟；
  slip roll 反向读 PCM，slip vinyl 以 Tape shadow 实现（手势尚未接入）。
- `jump::phase_preserving` — 目标帧 = 目标拍位 + `phase(当前) × 目标拍宽`,兼容非均匀网格
- `sync.rs` — **`Playhead { tempo, nudgerate, align, lock }`**,引擎拿到的是
  `playing_rate = tempo + nudgerate`:
  - `tempo` = fader / `sync tempo` / 组速度反算写的,**`sync unlock` 保留的就是它**
  - `nudgerate` = 相位修正(PLL/Linear)与 `nudge` 叠加的临时量,收敛或松手即归零
  - 每块 `apply_sync()` 在渲染**之前**跑;`push_rate()` **变化才下发**(控制器收敛后就不再写引擎)
  - 换流 `switch_to()` 后 `suppress()` 跳过一次控制器读数——位置跳变不是相位误差
- `PhaseAlign::{Instant, Linear, Pid}`:`Instant` 是换流跳位置(命令时执行完就清空,**不留**
  控制器,否则会自己去追那次跳转);`Linear` 定斜率 `err_sec/t`;`Pid` 是 PI(`kp`1.0 / `ki`0.2,
  积分抗饱和,输出 ±5% 限幅 = 收敛超时的兜底)。**相位差先包裹到 [-0.5, 0.5]、再换算成秒**
  (`err_beats × 60/bpm`):秒÷秒才是无量纲比率,增益与限幅才量纲自洽
  - **`nudgerate` 总限幅 ±15%** = PLL ±5%(收敛兜底)+ nudge ±10%(DJ 手掰),两者同向叠满也不越界
### `flow/`
- `Playing` — 一个独立播放声部：引擎 + 时钟 + `LoopRangeCell`。`playing` 是主/滑时钟，
  `shadow` 是 loop/roll/slip 的第二声部；切换都是块边界硬切，无 crossfade。
- `Direction::Reverse` — slip roll 用：反向读 loop range 的 PCM，Signalsmith 仍正向处理。
- `PitchShiftEngine` — 两套后端:`Tape`(零延迟 varispeed) / Signalsmith Stretch
  (`Keylock` 的窗口来自 live.toml `[keylock]`，默认 2048/512)。一个 2N 通道实例承载全部 stem；
  `prepare_jump` 用 `outputSeek` 对齐，loop wrap 只是喂入流内的 splice，引擎不复位
- **播放头是曲目帧,不是输出帧**:`output_frame_exact += capacity × ratio`(f64 累加,
  逐块取整不累积漂移)。数输出帧会让位置与 tempo 脱钩——波形、beatjump 的 `cued_from`
  补偿、loop 映射、相位同步在**任何非 1.0 速度**下都会读到错的钟。ratio=1.0 时两者逐位
  相同,所以 `phase_probe.sh` / `beatlock` 这类恒速回归看不出来;上游引擎自己也是拿
  `source_position()` 当播放头的

---

## hypermixx-library

分析管线:`Source → downmix_to_mono → backend → RawAnalysis → fit_rigid → BeatSpec
→ GridCompiler → TrackAnalysis → SetAnalysis`

- `BeatSpec` — 可编辑真源(段落化 BPM + 起点 + 拍数);刚性网格 = 单段
- `GridCompiler` — 三段编译:起点前填至 frame 0 / 段内等距 / 接缝去重 + 末段外推到轨尾
- `fit_rigid` — 中位数拍距 → 倍频归位(拉进 [60, 200] BPM)→ 整数拍号分配 + 最小二乘
- `TrackInfo` — 持 `BeatSpec` 与 key;`Mutex<Option<TrackAnalysis>>` 惰性编译,改 spec 即失效

---

## hypermixx-cli

启动参数:`--config <file>`(TOML 拓扑,自定义通道/链/输出)/ `--print-config`
(输出参考拓扑)/ `--backend auto|stratum|timestretch` / `--midi <port> [--midi-map <file>]`
(打开 MIDI 输入,缺省映射 `midi-map.toml`;端口名或序号,`midi ports` 可列);deck 数量启动时
从引擎查询(`channel_count`,自定义拓扑可配任意通道)。

会话命令(target-first:行首 `deck0`/`0`/`master` 选目标,缺省用焦点 deck):
- `load <path> [bpm]` — 后台解码;给 bpm 则建常网格跳过分析
- `analyse` — 后台跑 `library::analyser::analyze`,完成后 `SetAnalysis`
- `play / cue [play|back|set] / jump <frame> / beatjump <beats> / tempo / tempofader / temporange / keylock on|off / key / state / quit`
- `loop` 族:`loop in|out|<n>|exit|cancel|halve|double|edit <len|move|in|out> <beats>|quantum <q>`
  (手动 in/out 带量化与 looper 预喂,`<n>` 循环中 = 只调 out,halve/double = 当前长度
  ÷2/×2、域 1/32..64 拍,循环中编辑原地原子 store,exit 无缝续播越过 out)
- `fx` 族:`fx add|remove|list|set|on|off|trigger|pad press|release`,chain 地址
  `master`/`m`/`deck0`/`d0`/`0`;`fx help` 从注册表生成 kind/参数清单(永不与引擎脱节)
- `sync` 族:`sync tempo | phase <instant|linear [秒]|pid> | tempolock |
  phaselock <mode> [秒] | set-leader | unlock`;`nudge <delta> <秒> | nudge off`。
  解析错误(`linear 0`、非数字秒数)前端直接 `Failed`;引擎拒绝(无网格、反向同步)后到为 `Error`
- TUI deck 信息行多一枚黄色徽标:`sync phaselock 122.0 BPM ← deck0 phase pid locked`,
  与 CLI 的 `deck_line` 共用 `response::sync_badge`(所以测它就同时覆盖两边)
- `midi ports` — 列出 MIDI 输入端口(`--midi` 在启动时打开并把映射翻译成 `Command`,
  接线在 `src/midi.rs`:translator 线程 + 1ms 合并 flush + `ListFx` 名字解析)
- TUI 布局(`src/tui/view.rs`):顶部一行全局 header;deck 区合计不超过屏幕一半,每个 deck
  只保留一行状态(文件名在边框标题,右侧是 `已播放 / -剩余`)+ 波形(左声道在上、右声道在下的
  三频段镜像;缩放单位是 frames/cell;分频点默认 200/4000Hz，可在 preference 调);底部
  `browser : command = 2 : 1`,browser 内部再按 1 : 3 分栏(目录/歌单 | 曲目列表)。波形 backend
  按 preference 的 `auto`/`kitty`/`braille` 选择(auto 为 kitty → braille;
  三频段颜色也在 `profile/preference.toml`)。command
  上半是响应日志,下半输入带的顶线嵌入 `deck0 / deck1 / browser` 目标(`Tab` 循环;browser
  目前没有 CLI 前缀),目标文字与面板标题 `command` 左对齐
- TUI 内选择器(`src/tui/picker.rs`,两个前端共用):`--tui` 下 `F2` 选 MIDI 端口、`F3` 选映射
  文件、`load` 不带路径弹文件浏览器;`Load` 完成后 `App::connect_midi` 连上,`--midi/--midi-map`
  仅是免选择的快捷方式
- `--midi-guide [<file>] [--midi <port>] [--decks <n>]` — learn 模式地图编辑器(`src/tui/guide.rs`),
  **不启动引擎**;左目标清单/右原始事件监视器(PgUp/PgDn 滚动)/底部提示,键位
  `↑↓ Enter 空格 m x s p f q`,端口/文件也可在 TUI 里选;逻辑在 `hypermixx_midi::guide`(纯状态机)

### 脚本
- `scripts/phase_probe.sh` — 双 deck 反复 `beatjump`,看相位差**增量**是否恒定(跳转精度)
- `scripts/sync_phase.sh` — 双 deck **先后起播**制造相位差,再 `sync <mode>`,每拍采样,报
  **稳态相位差**(末 5 轮 |diff| 均值);mode = `pid|linear|instant|tempo|none`。
  起播差按**拍**给而不是按秒(按秒换个 BPM 可能恰成整数拍,相位差直接归零、看不出效果);
  等两路 `loaded:` 而非盲等固定时长;播放头没走一律判失败(全 0 不算通过)

响应由独立打印线程渲染(`thread::scope`,response channel 属于 pipeline),输入永不阻塞。

---

## 流式分离与渐进装载

整轨分离要等到最后一个窗口 ⇒ 第一次听到 stems 是几分钟后。改成**流式**：`CharonSeparator::stream_with_scale`
走 charon 的 `separate_stream`，它按模型自己的窗口网格扫轨道、**一块一旦不再被后续窗口影响就交出来**
（默认 overlap 0.25 时每块 = 一个 stride = 5.85 s 音频），并自己负责重叠相加与归一化。
**没有自造的 chunk/margin/对齐**：模型的窗口铺在轨道自己的网格上，输出与整轨路径**逐样本相同**
（charon 自己的保证，`tests/real_model.rs` 实测 worst sample difference = 0）。`shifts > 1` 时运行时
拒绝流式（时间平移只能整轨做），那条路退回整轨一块。

唯一保留的机制是**输入归一化**：`InputScale`（mono 参考的均值/样本标准差，Demucs 约定）在整轨上测一次，
经 `MixSource` 施加、在 `BlockAdapter` 里撤回。这不是洁癖——**续跑**时运行时只拿到轨道尾部，
若按尾部自己的统计归一化，接缝两侧就是从不同尺度的输入分离出来的（实测：Σ 差 −68 dB 但单条 stem 差 ~20 dB）。
续跑从水位往前退一个 segment 的 stride 边界开始（窗口仍在全局网格上），已落盘的块被丢弃。

装载是**一次**的：`media::ChunkedStreams` 是 n 条可增长的 `Source`（`ArcSwap` 快照，读端零锁），
deck 装好后每个块 append 即可被 `feed()` 读到 —— 不重启 flow、不重装、不停音乐。水位之外**回落**
（stream 0 放 fallback、其余静音，n 路之和不变）并在前沿 50 ms 淡化，所以跳到未装入区间也有声。
它**不认识 stems**：几个流、有没有 fallback 都是参数；将来的渐进装载/网络流可以原样复用。
`Source::available_frames()`（默认 = total）让引擎报 `StemStatus::separated`，`stem status` 显示百分比。
缓存同一 entry **按块追加**，`meta.frames` = 已完成水位 ⇒ 完整命中即纯回放（连模型都不打开），
部分命中即续跑。

## 采样率与 live 设置

引擎率是 **live 设置**（`profile/live.toml` 的 `[audio] sample_rate`，缺省 44.1 kHz）：
`media::decode_file_at(path, rate)` 在装载时重采样到它，`AudioPipeline::start_with(cfg, rate, tuning)`
把它和 ring/prefill 大小一起交给引擎，此后引擎内部没有采样率概念（`BeatGrid`/deck/FX 都只认帧）。
**stem 分离仍要求 44.1 kHz**（内置模型是 44.1k 导出、缓存 key 也按它），非 44.1k 时 CLI 直接拒绝
`stem separate`，不做双向重采样。`block_frames` 保持编译期常量：它是时间拉伸引擎的喂入契约，
不是本机性能参数。

### 历史：固定 44.1 kHz 的理由

引擎域固定 44.1 kHz:目标设备(ALSA `default`)原生 44.1 kHz,`preferred_config` 按引擎率
请求即直通。历史教训——引擎 48k 而设备 44.1k 时,ALSA 静默接受 48k 请求却按 44.1k 时钟
消费 → 慢放 8.8% + 音调低 8.8%。多设备自适应(设备率≠引擎率时在回调内重采样)是后续工作。

---

## 数据流

```
CLI  stdin
  │ spawn_load: decode → PcmPool ── Command::Load{source, analysis?} ──┐
  │ spawn_analyse: analyser::analyze → Command::SetAnalysis ───────────┤
  │ fx … ── Command::{AddFx,SetFxParam,…} ─────────────────────────────┤ crossbeam
  ▼                                                                    ▼
AudioPipeline ──producer thread──────────────────────────────────────────┐
  │ 命令分发(块边界): transport → deck / FX → Mixer::handle_fx_command   │
  │ 查询: deck_source / channel_count(命令空队列时也 drain)             │
  │ 每 tick: SyncGroup::prepare → make_ctx → Mixer::process              │
  │   ├ channel × N: deck.render_streams[1..4] → flow_fx[i] → fader[i] → Σ │
  │   │                     → deck_fx → deck_fader → gain                 │
  │   │              → master.sum;cue tap → cue.sum → blend(master)       │
  │   ├ master: fx → fader → limiter(安全级)                            │
  │   └ cue: fader                                                       │
  └─ Bus → Output.write(重映射+trim)→ ring → cpal 回调(44.1k 直通)→ 声卡
```
