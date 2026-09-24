# Hypermixx 架构

Rust workspace,六个 crate 单向分层:**core(类型) ← media(PCM) ← audio(引擎)**;
library(分析)与 audio 零交叉依赖,只被 cli 调用。引擎域 **44.1 kHz 立体声**(与目标设备
ALSA default 的原生时钟一致,cpal 回调直通,零转换)。

```
cli ──→ core / media / audio / library / midi
audio ──→ core, media          (+ timestretch, git 依赖 rev 锁定; serde + toml)
library ──→ core, media        (+ stratum-dsp, git 依赖 rev 锁定)
midi ───→ core                 (+ midir, serde + toml)
media ──→ core                 (+ symphonia)
core ──→ serde
```

```
crates/
├── hypermixx-core/       # 协议与类型(仅依赖 serde)
│   └── src/
│       ├── beatgrid.rs   # BeatGrid:绝对帧拍网格 + 相位/拍宽查询
│       ├── key.rs        # Key / KeyMode / KeyFormat(传统 + Camelot)
│       ├── analysis.rs   # TrackAnalysis { beatgrid, key, bpm }
│       ├── deck.rs       # DeckId = u8 / DeckState 快照
│       ├── command.rs    # Command / CommandResponse / Backend / FxChainId / FxSlotRef
│       └── source.rs     # Source trait + Shared = Arc<dyn Source>
│
├── hypermixx-media/      # PCM:解码 + 内存池(core + symphonia)
│   └── src/
│       ├── decoder.rs    # decode_file:symphonia → 44.1kHz 立体声 f32
│       └── pool.rs       # PcmPool:不可变 Arc 数据的 Source 实现
│
├── hypermixx-audio/      # 实时引擎(core + media + timestretch + serde/toml)
│   ├── src/
│   │   ├── ringbuf.rs    # rtrb SPSC 封装 + 自由函数
│   │   ├── pipeline.rs   # AudioPipeline:producer 线程 + 命令/查询通道
│   │   ├── deck/
│   │   │   ├── deck.rs      # Deck:流状态机 + cued_from 跳转补偿 + pull_into + apply_sync
│   │   │   ├── jump.rs      # Seek{Frames,Beats,Beat,Quantized} + phase_preserving
│   │   │   ├── flowshift.rs # FlowShift(原 TimeShift):后台流预热
│   │   │   ├── loop_.rs     # LoopRange/原子 cell/LoopSource(virtual→actual 映射读) + 量化
│   │   │   └── sync.rs      # Playhead(tempo+nudgerate)/PhaseAlign/Pll/Nudge/SyncCtx
│   │   ├── flow/
│   │   │   ├── flow.rs       # Flow:单次播放单元(状态机)
│   │   │   └── pitchshift.rs # PitchShiftEngine:timestretch 引擎包装
│   │   ├── sync.rs          # SyncGroup:谁领谁跟 + 每块预扫(编排,不进 mixer)
│   │   ├── mixer/         # 混音拓扑(见下)
│   │   └── fx/            # 效果子系统(见下)
│   └── tests/            # engine / beatlock / loop / sync / decode / tone_faithful
│
├── hypermixx-library/    # 分析与曲库(core + media + stratum-dsp)
│   └── src/
│       ├── beat_spec.rs     # BeatSpec/Segment:可编辑网格真源
│       ├── grid_compiler.rs # GridCompiler:前填/等距/接缝去重/尾外推 → BeatGrid
│       ├── track.rs         # TrackInfo:持 BeatSpec,惰性缓存编译结果
│       ├── waveform.rs      # 峰值概览
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
    └── src/main.rs       # 行解析 / fx 命令族 / 后台 decode+analyse / 打印线程
```

### 外部依赖(git,rev 锁定)

| crate | 来源 | 锁定 |
|---|---|---|
| `stratum-dsp` | `github.com/HLLMR/stratum-dsp`(第三方库) | `rev = 758e0b6` |
| `timestretch` | `github.com/robmorgan/timestretch-rs`(经 gh-proxy 镜像地址) | `rev = 2628090` |

不随本仓库分发;升级 = 改 rev + `cargo update -p <crate>`。
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
- `Play` / `Pause` / `Jump { target_frame }` / `BeatJump { beats }`
- `SetRate` / `SetProfile` / `SetAnalysis` — 分析结果从这里进入 deck,**没有 `Analyse` 命令**
- sync 族:`Sync { deck_id, op }`,op = `Tempo` / `Phase{mode,t}` / `TempoLock` /
  `PhaseLock{mode,t}` / `SetLeader` / `Unlock`;`PhaseMode = Instant | Linear | Pid`。
  **`phase` 包含 `tempo`,`phaselock` 包含 `tempolock`**;`SetLeader` 不带参数(target-first
  语法下 `deck0 sync set-leader` 即“deck0 是 leader”)
- `Nudge { deck_id, op }`,`op = Start{delta, seconds} | Stop` — 临时速率弯折,
  只动 `nudgerate`,结束时 `tempo` 原封不动
- `DeckState` 除 transport 外还报 `tempo` / `nudgerate` / `playing_rate` / `lock` / `align` /
  `nudge` / `sync_leader` / `sync_mode` / `group_bpm` —— TUI 徽标与 `sync_phase.sh` 的判据都读它
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
deck.pull_into(bus)
  → flow_fx        逐流插入(换流即重置滤波记忆)
  → [cue tap]      PostFlowFx
  → flow_fader     逐流电平
  → [cue tap]      PostFlowFader
  → deck_fx        逐 deck 音色(跨跳转持久)
  → [cue tap]      PostDeckFx
  → deck_fader     (stems 预留,现恒 unity)
  → [cue tap]      PostDeckFader
  → crossfader     按侧别的 pan law(Left/Right/Center × EqualPower/Linear)
```

`MasterBus::render`:`sum → master_fx → master_fader → limiter(安全级)`。
四样东西**永远不可配置**:

1. limiter 是 `MasterBus` 的字段而非槽位——配置忘写 `"limiter"` 也不可能把全尺度音频
   送到 DAC(且不走链跳过优化,engaged 即执行)
2. cue tap 的四个位置是枚举,不是自由路由
3. crossfader 恒在最后(路由决策,不该因 fader 移动改音色)
4. 阶段顺序本身

其余由 `MixerConfig` 纯数据描述(`config.rs`):每通道的 flow_fx/deck_fx 名单
(`Vec<String>`,经 `FxKind` 注册表解析,未知名 → `MixerError::UnknownFx` 构造失败)、
fader 起始位、cue_send/cue_tap/side/curve、master fx/limiter/fader、输出列表
(name/role(Main|Headphones)/channels/gain)。

- `Bus` — 面内(L/R 分离)块单位,构造时分配,热路径零分配;效果器逐面迭代省去步长乘法
- `Channel` — 拥有自己的 deck、两条链、四个 fader;**flow/deck 两链对外是一个合并索引
  空间**(`slot(i)` 先 flow 后 deck,`AddFx` 落 deck 半边)
- `Output`(`output.rs`,crate 里唯一出现 cpal 类型的地方)— 每输出独立 ring + stream;
  回调外预分配 scratch、partial push 永不阻塞、声道重映射安全钳制(单声道取和);
  `gain` 逐目的地 trim;**预填半 ring 静音**(~46ms)避免启动竞速;producer 侧 overrun
  有计数(callback 侧 underrun 与真实静音不可区分,故不数)
- **同一物理设备只保留一条流**:main+headphones 都落在 default device 时,PipeWire 会在
  graph 里把两条流直接相加——main 有 limiter(≤−1 dBFS)但 cue 总线没有(cue_send 1.0
  时电平可达 2.0),DAC 实收 ~2.9 → 硬削波。Mixer 检测设备名重复,第二条降级 sink 不路由
  (设备选择 `[[output]] device` 字段是后续工作)
- `simple_dj()` — 参考拓扑:2 通道(各 eq+filter,cue_send 1.0,tap PostDeckFx)+
  master limiter + main/headphones 输出;`reference_toml()` 是它的 TOML 文本
  (`--print-config` 输出,测试钉死解析回 `simple_dj()`)

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
  钳制、发散自愈 + 计数;`bipolar_amp` 双极性 fader 律(−1 恰好静音)
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
- `Deck` — 单 source、单活跃流;`jump()` 记 `cued_from`(**virtual 时钟**),`switch_to()` 把新流
  落点前移预热期间已播放的帧量,保证跨 deck 相位锁定(`phase_probe.sh` 的 `err` 恒 +0);
  `pull_into(&mut Bus, &FxContext)` 是 `process_block` 的面内薄包装
- **两个时钟**:`virtual_frame()` = 引擎播放头,只增不减、loop 不碰(slip);
  `current_frame()` = `map(virtual)` = 听到的位置(波形/state/beatjump 都读它)
- **Loop 状态机**:`loop_range` 挂在 Flow 上(切流即消亡,从不原地清);手动 in 生成
  **LoopFlow**(预热流,存于 deck 而非 flows 向量)——预热完后**与主流逐块同速跑、输出丢弃**,
  每块按“此刻按 out 会得到的区间”更新临时 range,`out` 按下存的就是该值(no-op store)⇒
  主流 retire、LoopFlow 原地转正,**无 cued_from、零延迟**;beatloop 不跟跑(建流即带 range,
  预热完经 FlowShift 换流);`loop <n>` 循环中=只调 out(halve/double);循环中编辑=整段
  Range 一次原子 store(不换流);退出 = 按下即预热换流、**落点 = 旧流输出停止处**
  (`pending_exit_resume`)——无缝:当前圈自然播完、越过 out 续行;旧 range 随流消亡;
  slip 落位是单列功能 `slip loop`;LoopFlow 驱动遇换流重锚时钟走 **rebase 分支**
  (倒退/大步 → `reset_to` 对齐)
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
- `FlowShift` — 后台预热线程,双通道:**jump 通道**保留 supersede(快跳只认最新);
  **LoopFlow 通道**双向豁免(`loop in` 循环中 = exit 换流 + 新 LoopFlow 同时在飞);
  被作废的 LoopFlow 记入 reap,通报时回收,不漏在 warm map 里

### `flow/`
- `Flow` — `Preparing → Ready → Active → Retired`;活跃时经引擎采样,暂停/耗尽补静音;
  持有自己的 `LoopRangeCell`,`render()` 无状态门(LoopFlow 的同速驱动用它),
  `prepare()` 与 `reset_to()` = seek 协议 + **排空 warm-start 锄热**——宣布 ready 与
  换流后的第一块都已是收敛音频(任何切换都不呼吸锄热间隙,换流无缝)
- `PitchShiftEngine` — timestretch-rs 三 Profile:**Tape**(零延迟直通)/ **Keylock** /
  **WideKeylock**(预热走 `reset → set_track_position → warm_start`);`feed()` 按
  `demand_hint` **按需喂入**(超前量 32768→~1100 帧 ≈25ms,loop_range 变更影响窗口小到
  可被 overlap 淡化),按 LoopRange **分段读 + 每段 `set_track_position(实际位置)`**——
  wrap 只是喂入流内的 splice,引擎永不复位(Keylock 下无瞬态)
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
- `play / pause / jump <frame> / beatjump <beats> / rate / profile / state / quit`
- `loop` 族:`loop in|out|<n>|exit|cancel|halve|double|edit <len|move|in|out> <beats>|quantum <q>`
  (手动 in/out 带量化与 LoopFlow 锄热,`<n>` 循环中 = 只调 out,halve/double = 当前长度
  ÷2/×2、域 1/32..64 拍,循环中编辑原地原子 store,exit 无缝续播越过 out)
- `fx` 族:`fx add|remove|list|set|on|off|trigger|pad press|release`,chain 地址
  `master`/`m`/`deck0`/`d0`/`0`;`fx help` 从注册表生成 kind/参数清单(永不与引擎脱节)
- `sync` 族:`sync tempo | phase <instant|linear [秒]|pid> | tempolock |
  phaselock <mode> [秒] | set-leader | unlock`;`nudge <delta> [秒] | nudge off`。
  解析错误(`linear 0`、非数字秒数)前端直接 `Failed`;引擎拒绝(无网格、反向同步)后到为 `Error`
- TUI deck 头行多一枚黄色徽标:`sync phaselock 122.0 BPM ← deck0 phase pid locked`,
  与 CLI 的 `deck_line` 共用 `response::sync_badge`(所以测它就同时覆盖两边)
- `midi ports` — 列出 MIDI 输入端口(`--midi` 在启动时打开并把映射翻译成 `Command`,
  接线在 `src/midi.rs`:translator 线程 + 1ms 合并 flush + `ListFx` 名字解析)
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

## 采样率约定

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
  │   ├ channel × N: deck.pull_into → flow_fx → fader → deck_fx → fader │
  │   │              → crossfader → master.sum;cue tap → cue.sum         │
  │   ├ master: fx → fader → limiter(安全级)                            │
  │   └ cue: fader                                                       │
  └─ Bus → Output.write(重映射+trim)→ ring → cpal 回调(44.1k 直通)→ 声卡
```
