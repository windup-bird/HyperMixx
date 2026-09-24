# MIDI 映射设计(v1)

状态:M1/M2/M3/M4 已落地。范围:**class-compliant MIDI 输入映射 + mixer fader 命令补齐**。
LED/灯反馈、MIDI clock 同步、hot cue 独立命令明确不在 v1(见文末)。

```
控制器(MIDI 字节流)
  │ midir 输入回调线程
  ▼
hypermixx-midi: msg 解析 → translate(纯函数 + latest-wins 合并)
  │ crossbeam command_tx(多生产者,与 REPL/TUI 并列)
  ▼
AudioPipeline producer:块边界 route → Mixer / Deck
```

---

## 1. 现状与缺口

地基已就绪的部分:

- `command_tx` 是 crossbeam **多生产者**,REPL/TUI 各占一个,MIDI 线程直接再 clone
  一个,**音频线程零改动**;
- 命令在 producer 的**块边界**统一应用,连续量走 `Param` 双原子 + 指数平滑,MIDI 的
  7-bit 阶梯值天然不会咔哒;
- `CommandResponse::Ok` 前端渲染为空(`response.rs`),高频命令不刷屏——但 `Error`
  会打印,所以映射层必须只发合法目标。

缺口(M1 补齐):

| 缺口 | 现状 | 方案 |
|---|---|---|
| **混音推子/交叉推子无命令** | `set_flow_fader` / `set_crossfader` / `set_cue_send` / master `set_fader` 只能靠 TOML 启动配置 | core 新增 `Command::SetFader`,`pipeline::route` 加 arm(mixer 侧 setter 均为 `&self`,无需 `&mut`) |
| 推子位置无回读 | `Channel::levels()` 存在但无 query 暴露 | v1 **不需要**:soft-takeover 只比对映射层自己的镜像(初始值取自 `MixerConfig`),零协议成本 |
| FX 参数按 index 寻址 | `SetFxParam { slot: FxSlotRef{index} }`,index 随 `RemoveFx` 漂移 | 映射文件按**名字**写(`chain/ fx / param`),启动时经 `ListFx` 解析成 index 注入 map,复用 cli 的 `Slots` 簿记 |
| 无 cue 点概念 | 只有 `Jump` + `Play` | 映射层组合(hot cue = 记录 frame + `Jump`);不够用再补 `Command::Cue` |

---

## 2. 技术选型

| 层 | 选择 | 理由 |
|---|---|---|
| MIDI I/O | **`midir` 0.11** | ALSA seq 后端,`alsa-sys` 经 cpal 已在依赖树里(**零新 C 依赖**);输入输出同一 API,将来 LED 反馈不换库;2026-04 仍活跃维护 |
| 消息解析 | **手写 ~150 行**(`msg.rs`) | 只需 NoteOn/Off、CC(绝对 + rel1/2/3)、PitchBend,外加过滤 0xF8 clock / 0xFE active-sensing。`midi-msg` 0.9 是 edition 2024(要求 rustc ≥ 1.85,README 承诺 1.70+),为 150 行解析引 8k 行依赖不值 |
| 落点 | **新 crate `hypermixx-midi`**,仅依赖 `core` + `midir` + serde/toml + crossbeam | 翻译是纯函数 `translate(&Event, &Map) -> Vec<Command>`,无硬件可单测;维持五层单向分层 |
| 映射定义 | **TOML 静态文件**(`deny_unknown_fields` + 坏键报行号,抄 `MixerFile` 风格) | 可版本控制、可单测;`MidiGuide` TUI 只是**生成/编辑它的工具**,不引入运行时可变状态——TOML 永远是唯一真源 |
| Guide 形态 | **TUI**(`--midi-guide`),复用 ratatui 0.30 + crossterm 0.29 | 与 `--tui` 同栈,零新依赖;guide 的状态机留在 `hypermixx-midi`(纯逻辑、可单测),渲染在 `cli/src/tui/guide.rs`,midi crate 不依赖 ratatui |

被否的备选:

- 直接用 `alsa` crate 手写 seq——零新依赖但 Linux-only、样板极多,与 README 的
  "跨平台"目标相悖;
- `portmidi` / `libremidi`——C FFI,无收益;
- JACK MIDI——要求用户跑 jackd;
- `midi-msg`——见上,rustc 版本要求过高;
- MIDI learn 作为运行时状态——需要运行时可编辑映射 + 持久化 + 双前端同步,首版
  过重;learn 只做 TUI 向导,产出物是文件。

---

## 3. M1 — 协议补齐(core + audio)

### `Command::SetFader`

单个 variant 承载一族,照 `LoopOp` / `SyncOp` 的既有风格:

```rust
// hypermixx-core::command
pub enum FaderTarget {
    Flow(DeckId),      // −1.0..=1.0 通道电平
    Deck(DeckId),      // −1.0..=1.0 stems 预留,现恒 unity
    CueSend(DeckId),   //  0.0..=1.0 线性(cue 总线电平语义,不是双极推子)
    Crossfader,        // −1.0..=1.0 广播到所有 channel(按 side 分侧是 pan law 的事)
    Master,            //  0.0..=1.0
    Cue,               //  0.0..=1.0 cue 总线推子
}

Command::SetFader { target: FaderTarget, value: f32 }
```

- 数值域由 kind 决定,命令层不解释;mixer 现有 setter 自带 clamp / 有限性消毒;
- `Crossfader` 是**一个**物理推子:`route` 里遍历所有 channel 写同一 position;
- 响应:成功 `Ok`(前端静默),deck 越界 → `Error(unknown_deck(...))`;
- `Command` 的手写 `Debug` impl 补一个 arm。

### `pipeline::route` 加 arm + 测试

- 穷举 match 会让编译器强制所有调用方表态,零回归风险;
- 测试:levels round-trip(发命令 → 读 `Channel::levels()`)、越界报 `Error`、
  crossfader 广播两侧、cue_send 域外值被 clamp。

---

## 4. M2 — `hypermixx-midi` crate

```
crates/hypermixx-midi/src/
├── lib.rs
├── msg.rs        # 字节流 → 事件;velocity 0 归一为 NoteOff;丢 clock/active-sensing/aftertouch
├── map.rs        # TOML schema;target manifest;解析 + 校验
├── translate.rs  # 纯函数 Map::translate + latest-wins 合并(1ms tick flush)
├── guide.rs      # guide 状态机(纯逻辑,渲染归 cli)
└── ports.rs      # midir 端口枚举/打开/回调线程 → Sender<Received>{time,event}
```

### `map.rs` — TOML schema

```toml
# midi-map.toml
[meta]
name = "my-controller"

[[bind]]
type  = "cc"            # cc | note | bend
mode  = "abs"           # abs | rel1 | rel2 | rel3(相对编码器各厂回绕规则不同,per-binding)
channel = 0             # 0-based;省略 = 任意
id    = 7
deck  = 0               # 或 omit(全局目标)
action = "fader.flow"   # 见下方 action 注册表
# 可选:value 映射
min = 0.0
max = 1.0
curve = "linear"        # linear | sharp(指数)

[[bind]]
type = "note"
channel = 0
id = 0x30
deck = 0
action = "play"         # 按钮类:NoteOn 触发、NoteOff 视语义(toggle / momentary)
```

- `deny_unknown_fields`,解析错误带行号(与 `MixerFile` 同风格);
- **action 注册表是单一真源**:guide 的清单、TOML 校验、`translate` 分支三者共用
  同一张表(`TargetManifest::standard(decks)`),永不脱节——对齐 `fx help` 从注册表
  生成的做法;
- `action` v1 集:
  - transport:`play` / `pause` / `beatjump±`(相对按钮步长可配)
  - fader 族:`fader.flow` `fader.deck` `fader.cuesend` `fader.cross` `fader.master` `fader.cue`
  - tempo:`rate`(CC → `SetRate`,带 ±% 量程,默认 ±8%,映射层做量程换算)
  - nudge:`nudge` — NoteOn → `Nudge::Start{delta, None}`,NoteOff → `Nudge::Stop`(momentary 天然对上)
  - loop:`loop.in` `loop.out` `loop.exit` `loop.cancel` `loop.halve` `loop.double`
    `loop.beat8`(N 可配 → `LoopOp::Beats(n)`)
  - sync:`sync.tempo` `sync.phase` `sync.phaselock` `sync.tempolock` `sync.leader`
    `sync.unlock`
  - fx:`fx.param`(chain + fx 名 + param 名 → 启动时解析成 `FxSlotRef`)`fx.on`
    `fx.off` `fx.toggle` `fx.pad`(→ `PadPress`/`PadRelease`)`fx.trigger`

### `translate.rs` — 纯函数 + 合并

- `translate(&Event, &ResolvedMap) -> Vec<Command>`:所有状态(相对编码器累加器、
  pickup 镜像)封装在 `TranslateState` 里,表驱动单测覆盖每个 action;
- **latest-wins 合并**:每个 target 只保留最新值,~1ms tick 统一 flush。推子快扫 =
  每秒上千 CC → 合并后每 target 每毫秒最多一条 `Command`。`Ok` 本身无害,但会白烧
  producer 的 drain 循环;中间值丢弃无所谓,**终值必达**——这是正确性而非限流;
- **soft-takeover(pickup)**:绝对控件首次触碰时,物理值未越过"上次发出的镜像值"
  则丢弃,越过才接管。镜像初始值从 `MixerConfig` 读。**不需要**引擎回读协议;
- 相对编码器:`rel1`(64 回绕)/ `rel2`(65/63)/ `rel3`(1..63 增 65..127 减),
  各厂不一所以 per-binding 配。

### `ports.rs` — midir 接线

- `list_ports() -> Vec<String>` / `open(name_or_index, Sender<Command>)`;
- midir 回调线程收字节 → `msg` 解析 → `translate` → `command_tx`;
- 端口不存在:**启动即干净退出**(与 `--config` 坏文件同一哲学);
- 运行中设备拔出:回调线程结束 → 发 `Notice` 到打印线程,进程不崩。

---

## 5. M3 — CLI 接线

- 新 flag:
  - `--midi <端口名|序号>` — 打开输入端口;
  - `--midi-map <file>` — 映射文件(与 `--midi` 同给才生效,或 `--midi` 缺省
    `midi-map.toml`);
  - `--midi-guide <file>` — 进入 guide TUI(**不启动音频引擎**,见 §6);
  - `midi ports`(REPL 子命令)— 列出端口枚举。
- `main.rs` 在分支到 repl/tui **之前** spawn MIDI 线程(只需 `command_tx` + decks +
  resolved map),与两个前端并列,不动它们;
- FX 名字 → index 的解析复用 `Dispatcher::prime` / `ListFx` 流程,解析结果注入 map;
- 生命周期:先 drop `MidiInputConnection` 再发 `Command::Quit`(照
  `AudioPipeline::Drop` 的教训——调用方还握着 `command_tx` clone 时等断开会死锁)。

---

## 6. M4 — `MidiGuide` TUI

形态:`hypermixx-cli --midi-guide [<map.toml>] [--midi <port>]`(**不需要声卡、不启动
引擎**,只需 MIDI 输入 + 文件读写)。路径可省:缺省 `midi-map.toml`;端口/文件都可在 TUI
内的选择器里改,所以最小启动只需 `--midi-guide`。

### 布局(ratatui)

```
┌─ 待配目标 ──────────────┬─ 原始事件监视器 ────────────┐
│ ▶ deck0  play      (—)  │ 12:03:41  CC  ch1 #7  = 64 │
│    deck0  pause     (—)  │ 12:03:41  CC  ch1 #7  = 65 │
│    deck0  fader.flow(✓) │ 12:03:42  NOTE ch1 #48 on  │
│    deck0  eq.low    (✓)  │ 12:03:42  NOTE ch1 #48 off │
│    …                     │  …滚动保留最近 N 条         │
├─ crossfader  (—) ────────┤                            │
│ master  …                 │                            │
└──────────────────────────┴────────────────────────────┘
│ 状态:等待按压 — 请拨动 "deck0 fader.flow"              │
│ 检出:CC ch1 #7 abs   [空格]确认  [m]切换模式  [x]清除  │
└────────────────────────────────────────────────────────┘
```

- 左栏:目标清单(来自 `TargetManifest::standard(decks)`)+ 当前绑定状态
  `(✓)` 已绑定 / `(—)` 未绑定 / `(✎)` 本次会话有未保存改动;
- 右栏:**原始事件监视器**——不经过映射,直接显示所有入站事件,解决"我怎么知道
  这个旋钮发的是几号 CC"这个首要问题;
- 底栏:当前动作提示 + 检出的候选绑定。

### 交互

| 键 | 行为 |
|---|---|
| `↑` `↓` | 选择目标(只在清单内移动,跳过只读分组标题) |
| `Enter` | 进入 armed 态:捕获**下一个**入站事件作为候选绑定 |
| `空格` | 确认候选,写入内存中的 map |
| `m` | 循环切换 mode:`abs → rel1 → rel2 → rel3`(相对编码器手动纠偏) |
| `x` | 清除该目标的绑定(或放弃当前候选) |
| `s` | 保存:临时文件 + 原子 rename 覆盖 `<map.toml>` |
| `p` | 打开端口选择器(Enter 连接,Esc 取消) |
| `f` | 打开文件选择器切换映射文件(Enter 加载,Backspace 上级目录) |
| `PgUp` `PgDn` | 原始事件监视器上下滚动(回到底部自动跟随) |
| `q` | 退出;有未保存改动先弹确认条 |

### 自动推断

- 事件类型/通道/编号直接来自检出;
- `type = note` 且 action 为 momentary(nudge/pad)时自动按 NoteOn/Off 配对;
- 相对编码器**自动探测**:armed 态下若连续值差为 ±1 回绕(如 127→0),提议
  `rel1`;不确定就落 `abs`,由用户按 `m` 纠偏;
- 确认时若该目标已有绑定,替换之(向导即编辑器,不是只能增)。

### 分层

- `hypermixx-midi::guide` — 状态机:`Guide::armed()` / `Guide::feed(&Event) ->
  GuideStep` / `Guide::confirm()` / `Guide::to_toml()`。**纯逻辑、无 I/O、可单测**;
- `cli/src/tui/guide.rs` — 只做渲染与按键翻译,midi crate 不依赖 ratatui;
- 已存在的 `<map.toml>` 启动即加载,清单显示现有绑定,重跑向导 = 增量编辑。

---

## 7. 测试计划

**单元(无硬件)**

- `msg`:字节级用例表——NoteOn velocity 0 归一、running status、clock/0xFE 过滤、
  三字节截断;
- `map`:合法 TOML round-trip;坏键/未知 action 报行号;manifest 与 action 注册表
  一致性;
- `translate`:表驱动 `事件 → 期望 Command`(每个 action 至少一条);相对三模式
  回绕;pickup 接管时序;latest-wins 合并(一串 CC 只留终值);
- `guide`:状态机走完一个完整配对流程;`to_toml` 产物能被 `map` 重新解析。

**集成**

- M1:route arm 的 levels round-trip / 越界 `Error` / crossfader 广播;
- 端到端:内核 `snd-virmidi` 虚拟回环 + `scripts/midi_probe.sh`(发 CC → 读 `state`
  判据,风格对齐 `phase_probe.sh` / `sync_phase.sh`),CI 无硬件可跑。

**回归**:现有 `phase_probe.sh` / `sync_phase.sh` 不受影响(`Command` 穷举 match 由
编译器兜底)。

---

## 8. v1 明确不做

- **LED / 灯反馈(输出)**:`response_rx` 是 crossbeam Receiver 克隆 = **MPMC 抽取式,
  不是广播**,MIDI 线程再 clone 会从 TUI 打印线程嘴里抢消息。需先在前端做扇出层
  ——独立工程,midir 输出端口能力已预留;
- **MIDI clock 同步**:牵动 `BeatGrid` / tempo 语义,独立大特性;
- **CUE 点 / hot cue**:先在映射层用 `Jump` + `Play` 组合,不够用再补 `Command::Cue`;
- **多映射文件热切换**:重启进程即可。

---

## 9. 风险与坑

1. **推子扫动洪水** → translate 层 latest-wins 合并(§4),不是限流;
2. **响应通道 MPMC** → v1 避开反馈;映射层保证只发合法目标,`Error` 不会污染 REPL;
3. **相对编码器各厂不一** → per-binding `mode` + guide 自动探测 + `m` 手动纠偏;
4. **MIDI 线程退出顺序** → 先 drop connection 再 `Quit`(§5);
5. **rustc 版本** → 不引 edition 2024 依赖,保住 README 的 1.70+ 承诺。

---

## 10. 里程碑

| # | 内容 | 验收 |
|---|---|---|
| M1 ✅ | `Command::SetFader` + route arm | `mixer::tests::set_fader_addresses_each_target_and_clamps` + `engine.rs::set_fader_reaches_the_mixer_through_the_command_channel`,全绿 |
| M2 ✅ | `hypermixx-midi` 骨架(msg/map/translate/ports) | 31 项无硬件单测覆盖全 action 表、pickup、相对编码器、latest-wins |
| M3 ✅ | CLI 接线(flag / 线程生命周期 / FX 名解析) | `--midi <port> [--midi-map <file>]` 真控制器插上即能推子、按键;`midi ports` 列表;坏端口/坏 map 干净退出;translator 线程用合成事件单测 |
| M4 ✅ | `MidiGuide` TUI | `--midi-guide [<file>] [--midi <port>] [--decks <n>]` 不启引擎;端口/文件在 TUI 内选;向导产出 TOML 经 `Map` 回验证后被 `--midi-map` 消费;pty 冒烟渲染+保存+退出通过 |

### 实现注记(M1/M2 落地时的修正)

- `FaderTarget` 枚举承载 `SetFader` 的六个目标;`Master`/`Cue` 和 `Flow`/`Deck`/`Crossfader`
  一样是**双极**推子(`0.0` = unity,引擎 `Fader` 本就是双极),只有 `CueSend` 是线性电平。
- `ports.rs` 回调产出 `Received { at, event }` 而非 `Command`:run 模式的 translator 线程与
  guide 的原始监视器共用同一份端口代码,监视器天然带时间戳(M3 接 translator,M4 直吃事件)。
- `Action::Fx` 的所有槽级动作(`On/Off/Toggle/Pad/Trigger`)都携带 `chain` + `fx`,因为索引
  必须由 CLI 经 `ListFx` 解析后注入,文件里只写名字(`Map::resolve_fx`)。未解析的 FX 绑定
  静默跳过,绝不发非法 `FxSlotRef`。
- `loop.beat` 需 `beats` 字段;`loop.beat<N>`(如 `loop.beat8`)作前缀别名兼容。
- `sync.phase` / `sync.phaselock` 单击默认 `PhaseMode::Pid`(收敛后校正归零,最适合单键)。
- guide 状态机(`guide.rs`)与 `Map::to_toml` 属 M4,本次未实现。

M3 追加:

- `ports::open` 返回**不透明** `Input`(内含 `MidiInputConnection`),`midir` 不进 cli 依赖树;
- translator 线程与 `MergeBuffer` 落在 `cli/src/midi.rs`(midi crate 保持引擎无关,只认 `Command`);
  1ms 节奏 flush 用**已过时长**判断而非纯阻塞超时,持续事件流不会饿死 flush;
- FX 名解析在启动时一次性完成(`ListFx` + `resolve_fx`),未解析的绑定发 `Notice` 后禁用;
- 生命周期:`MidiSession` 在 pipeline 之后声明,倒序析构 → **先关端口、join translator,再拆
  pipeline**;`Drop` 里先 `take()` 连接再 `join`,join 永不等活输入;
- 新增启动参数 `--midi` / `--midi-map`(缺省 `midi-map.toml`,`--midi-map` 单独出现报错),REPL/TUI
  共享 `midi ports` 子命令;仓库根提供示例 `midi-map.toml`。
- 修正:soft-takeover 的 mirror 命中判据加**半个 CC 步长的死区**——否则居中推子发 CC 64
  (`0.5039`)永远碰不到 mirror `0.5`,unity 对 unity 无法接管。

M4 追加:

- `map.rs` 的 raw 类型(`MapFile`/`RawBinding`/...)加上 `Serialize` + `skip_serializing_if`,向导直接
  编辑 raw 形式(不经过有损的编译形式),`Guide::to_toml` 序列化后再用 `Map::from_toml_str` 回验证,
  **保存的文件保证可加载**;
- `guide.rs` 纯状态机:`arm/feed/cycle_mode/confirm/clear/to_toml`;相对编码器自动探测(相邻 CC
  全幅回绕→`rel1`);重绑保留既有 FX 的 `chain/fx/param`(向导无引擎,无法枚举链/槽;新建 FX
  绑定时报错提示补全);全局目标(FX)匹配忽略 `deck`;
- `cli/src/tui/guide.rs`:左目标清单(`✓` 已绑 / `—` 未绑 / `✎` 本次改动)/右原始事件监视器/底部提示;
  键位 `↑↓ Enter 空格 m x s q`;`s` 临时文件 + 原子 rename;dirty 时 `q` 需再确认;
- `main.rs` 在读取拓扑**之前**就分支到 guide,真正零引擎依赖;端口/映射文件缺省时在 TUI 里
  选(单端口自动连),目标清单宽度由 `--decks`(缺省 2)给出。

UX 迭代(后续):

- 主 `--tui` 也接了 `src/tui/picker.rs` 的共享选择器:`F2` 选 MIDI 端口、`F3` 选映射文件、
  `load` 不带路径弹文件浏览器;连接逻辑收在 `App::connect_midi`(替换旧 session 即关旧端口);
- guide 原始事件监视器支持 `PgUp/PgDn` 滚动(带 offset 指示,回底自动跟随);
- deck 边框改用**每 deck 一个色相**的调色板(不再用灰色表示非焦点),焦点用**加粗**区分。
