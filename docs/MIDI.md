# MIDI 映射文件参考

一个映射文件把「MIDI 事件」翻成引擎的 `Command`。schema 与校验在
`crates/hypermixx-midi/src/map.rs`，`Event → Command` 的翻译在 `translate.rs`，learn 模式编辑器在
`hypermixx_midi::guide` + `crates/hypermixx-cli/src/tui/guide.rs`。

> 会话命令的语法见 [`COMMANDS.md`](COMMANDS.md)。本文只讲映射文件本身。
>
> **`action` 就是一条去掉目标的会话命令**：`"play"`、`"beatjump 4"`、`"sync phase"`、
> `"loop 1/4"`、`"fx param value"`。目标由 unit 的 `deck`（或这一行的 `deck`）提供。

---

## 1. 三层结构

```text
unit      一组物理控件（一个 deck 的一条通道、一块 pad 阵列、一个全局面板）
  control 一个 slot：类型（type）+ 硬件事实（endpoint）+ 角色（role）
  mode    一张 slot → action 表
```

- **`type`** 是 slot 的物理种类：`Button`、`Knob`、`BipolKnob`、`RelKnob`、`JogWheel`。
- **`endpoint`** 是硬件事实：MIDI 消息、通道、控制器号、编码方式。
- **`role`** 说明这个 slot 做什么：发命令（`action`，默认）、切模式（`mode_trigger` /
  `mode_cycle`）。
- **preset** 是「slot 名 + 类型 + 默认 mode 表」的模板；unit 只负责绑定端点。

## 2. 文件骨架

```toml
[meta]
name = "my-controller"          # 可选，仅用于标识
device = "DDJ-400:DDJ-400 MIDI 1 24:0"
description = "two decks, one pad grid"

[[unit]]
id = "deck1.mixer"              # 全局唯一
preset = "mixer"                # 可选
deck = 1                        # 1-based；省略 = 全局 unit
start = "default"               # 可选：起始 mode
mode_order = ["default"]        # 可选：把列出的 mode 排到前面

[[unit.control]]
slot = "filter"
endpoint = { midi = "cc", channel = 0, id = 12 }

[[unit.mode]]
name = "default"
[[unit.mode.bind]]
slot = "filter"
action = "fx param value"
fx = "filter"
```

- 解析是 `deny_unknown_fields`：拼错的键是**报错 + TOML 行号**，不是静默忽略。
- deck 号在文件里是 **1-based**（`deck = 1` 是第一个 deck），引擎内部是 0-based。
- 一份文件里每个 `(midi, channel, id)` 只能属于一个 slot；14-bit 的 `lsb` 也算一个。

## 3. endpoint

```toml
[[unit.control]]
slot = "filter"
endpoint = { midi = "cc", channel = 0, id = 12 }
```

| 字段 | 说明 |
|---|---|
| `midi` | `note` \| `cc` \| `bend` |
| `id` | CC 号或 note key；`bend` 省略 |
| `channel` | 0..=15（文件里 0-based，与 MIDI 线号一致；guide 列表显示的是 1-based 的 `ch1`） |
| `encoding` | 仅 `cc`：`absolute`（默认）\| `offset64` \| `twos_complement` \| `direction` \| `two_point` |
| `lsb` | 14-bit CC 对的低 7 位；`id` 此时是高位 |
| `reverse` | 相对编码反向 |
| `role` | 仅 `JogWheel`：`press` / `turn` |

一个 slot 通常只有一个 endpoint；`JogWheel` 要两个：

```toml
[[unit.control]]
slot = "jog"
endpoint = [
  { role = "press", midi = "note", channel = 0, id = 80 },
  { role = "turn", midi = "cc", encoding = "two_point", channel = 0, id = 6 },
]
```

### 编码

| `encoding` | 规则 | 典型设备 |
|---|---|---|
| `absolute` | `value/127` | 推子、绝对旋钮 |
| `offset64` | `value − 64` | 老的无限旋钮 |
| `twos_complement` | 1..63 = +，65..127 = − | 多数无限旋钮 |
| `direction` | 只看方向 | 每格值相同 |
| `two_point` | `1` = 一步 +，`127` = 一步 −，其余忽略 | 小控制器的 `{0,1,127}` 旋钮 |

`two_point` 是给「只发 1 和 127」的编码器用的，不要再靠 `twos_complement` 兼职。
判不准就用 `--guide` 的 raw 监视器看一遍，guide 的 learn 也会自动识别。

## 4. slot 类型

| `type` | 端点 | 能带的 action |
|---|---|---|
| `Button` | 1 个（note 或绝对 CC） | 任意非连续 action |
| `Knob` | 1 个绝对端点 | 连续 action；相对端点时可用 `nudge`/`vinyl` |
| `BipolKnob` | 1 个绝对端点 | 连续 action，默认 `min = -1, max = 1`，中心 = 0 |
| `RelKnob` | 1 个相对端点 | 连续 action，或 `action` + `down` 两个 edge action |
| `JogWheel` | `press` + `turn` | `vinyl`、`vinyl slip`、`nudge` |

## 5. mode bind

```toml
[[unit.mode]]
name = "default"

[[unit.mode.bind]]
slot = "fader"
action = "fader flow"

[[unit.mode.bind]]
slot = "filter"
action = "fx param value"
fx = "filter"
min = -1.0
max = 1.0

# RelKnob：一个方向一个 action
[[unit.mode.bind]]
slot = "choose"
action = "browser up"
down = "browser down"

# JogWheel：两半分别绑
[[unit.mode.bind]]
slot = "jog"
press = "vinyl slip"
turn = "vinyl"
```

| 字段 | 说明 |
|---|---|
| `action` | 主端点的 action（JogWheel 下等于同时给 press 和 turn） |
| `press` / `turn` | 只对 `JogWheel`：覆盖对应那一半 |
| `down` | 只对 `RelKnob`：反向 detent 的 action |
| `deck` | 覆盖 unit 的 deck（全局 unit 控制某个 deck 时才需要） |
| `min` / `max` | 归一化位置映射到的值域 |
| `curve` | `linear` \| `sharp` |
| `on` | `press`（默认）\| `release`；两边缘 action 不接受 |
| `chain` / `fx` | FX 的地址：链（`master`、`deck1`、`deck1/vocals`）与效果名 |

**action 的参数写在行内**，没有别的字段：

```toml
action = "loop 1/4"        # 分数拍长
action = "beatjump -8"
action = "nudge 0.01 0.02"
action = "vinyl 234"       # frames/tick = 1.8s × 引擎采样率 / 轮子每圈 tick 数
action = "fx param value"  # 参数名在行内，槽位名在 fx 字段
```

## 6. mode slot（模式键）

```toml
[[unit.control]]
slot = "mode-hotcue"
type = "Button"
role = "mode_trigger"
target = "hotcue"
endpoint = { midi = "note", channel = 0, id = 24 }

[[unit.control]]
slot = "mode-next"
type = "Button"
role = "mode_cycle"
endpoint = { midi = "note", channel = 0, id = 25 }
```

- 它们**不产生 command**，出现在 `[[unit.mode.bind]]` 里会报错。
- `mode_trigger` 必须有 `target`；`mode_cycle` 按 unit 的 mode 顺序前进。
- 没有绑定端点时只是没有接线，不算错误（guide 之后会学）。

## 7. preset

```toml
[[preset]]
name = "my-pad"
scope = "deck"                 # deck | global | both
start = "hotcue"

[[preset.slot]]
slot = "pad1"
type = "Button"

[[preset.slot]]
slot = "mode-hotcue"
type = "Button"
role = "mode_trigger"
target = "hotcue"

[[preset.mode]]
name = "hotcue"
[[preset.mode.bind]]
slot = "pad1"
action = "hotcue trigger 1"
```

内置 preset：`browser`(global) `crossfader`(global) `deckfx1` `deckfx2` `deckfx3` `jog`
`manualjump` `manualloop` `mixer` `outputfader`(global) `pad` `tempofader` `transport`。

- `browser` 只允许 global；slot 为 `choose` / `open` / `exit` / `load-1` / `load-2`。
- `manualloop` 的 `toggleloop` 用 deck 的 `loop_beats`；`double`/`halve` 改这个值，
  正在 loop 时同时重设当前 loop。`autoloop` 的转轮做同样的事。
- `manualjump` 的 `forward`/`backward` 用 deck 的 `jump_beats`；`double`/`halve` 改这个值，
  `autojump` 的转轮按当前步长向前/向后跳。
- `jump_beats` / `loop_beats` 默认都是 8，只存在于运行中的 deck 状态，不写 TOML。

- `pad` 有四个 mode：`hotcue` / `loop` / `beatjump` / `stems`，以及四个 trigger 和一个 cycle。
  `stems` 的 pad1–4 切换 drums/bass/other/vocals 的 mute，pad5–8 在该 mode 下留空。
- `deckfx1/2/3` 是三个通用 deck FX 段，slot 名为 `toggle` / `param1` / `param2`，分别引用
  `fx1` / `fx2` / `fx3`。参数按序号绑定（`fx param 1` / `fx param 2`），这样以后换掉指针
  指向的效果时 MIDI map 不用改。
- `mixer` 没有 `load` slot；`cue` 叫 `cue-toggle`。

用户 preset 放在 `[meta] presets = ["presets/my.toml"]` 列出的文件里（相对 map 所在目录）；
同名 preset 整体覆盖内置。

### preset 与 unit 的合并

1. slot 顺序：preset 的 slot 先，unit 新增的接在后面。
2. unit 的 `[[unit.control]]` 可以省略 `type`（用 preset 的）；写了必须一致。
3. mode：preset 的 modes 先；unit 同名 mode **整个替换**，新名追加。
4. `mode_order`（只写在 unit 上）把列出的 mode 排到前面，其余保持声明顺序。
5. `start`：unit > preset > 第一个 mode。

## 8. FX 的寻址

文件里 FX 只写**名字**（`chain`/`fx`/`param`），从不写槽位号：槽位会因增删移位，文件必须能跨重启。
CLI 启动时用 `ListFx` 拿到每条链的槽位名，再 `Map::resolve_fx` 把名字钉到索引上。

`chain` 省略时用 unit 的 deck，所以同一个 `mixer` preset 在 deck1 和 deck2 上各指各的链。

### 参数名与参数序号

- `fx param low`：按效果自己声明的参数名。
- `fx param 1`：按参数序号（1-based），适合 `fx1/fx2/fx3` 这种将来可切换效果的指针槽。
  引擎用效果当前的参数顺序解析；序号超出范围会报错，不会静默丢弃。

`fader` / `crossfader` / `gain` 不是 FX 槽：`fader flow`、`fader cross`、`fader gain` 是
通道 fader，和 `fx = "gain"` 这种插入效果分开。`deckfx1/2/3` 只控制 deck 链；stem 自己的
FX 链要用 `chain = "vocals"` / `chain = "deck1/vocals"`，这次不提供 stemfx preset。

`fx1` / `fx2` / `fx3` 是给未来可变 FX 指针预留的稳定槽位名。当前默认 `config.toml` 还没有
暴露这些名字，所以真实运行时会提示 unresolved 并跳过对应绑定；guide 仍可保存和编辑它们。

## 9. 打开与编辑

- `--controller <port>`：启动时打开端口并按映射翻译；映射文件由 config 的 `[[controller]].map` 指定（可声明多个，各自独立）。
  （旧的 `--profile` / `--live` / `--controller-map` / `--midi*` 拼写已移除；会话里 `controller ports` 仍接受 `midi ports` 别名。）
- TUI 下 `F9` 打开设置界面选端口与映射文件（取代旧的 `F2`/`F3`）。
- `--guide [<file>] [--controller <port>] [--decks <n>]`：learn 模式编辑器，不启动音频引擎。
  顶栏显示 file/controller；下面是带边框的 `deck | unit | slot`、`info | watch dog`、
  灰色键位提示和居中 command 按钮。

  导航：

  - `←→` 换栏，`↑↓` 换行；非当前栏保留灰色高亮。
  - `Enter`：deck/unit 下钻；`new` 上打开新 deck 菜单；`add unit` 上打开 preset 菜单；
    slot 上进入 learn。
  - `Tab` / `Shift+Tab` 进入并切换 command；`Esc` 回到栏目。
  - 任意方向键移动栏目时，command 焦点回到左一按钮。
  - `PgUp` / `PgDn` 回看 raw MIDI。

  command 固定六个：

  ```text
  [Enter | Learn | Confirm]  Delete  Connect  File  Save  Exit
  ```

  - deck/unit：左一是 `Enter`
  - slot：左一是 `Learn`；learn 中变成 `Confirm`
  - `Delete`：删除选中的 unit，或把选中的 slot 从 unit 解绑；preset 自带且没有 unit 覆盖的 slot 不删
  - `Save` 没有文件路径时打开文件选择器；底部可输入文件名，Enter 直接覆盖（测试用）
  - `new` 菜单固定两项：`clone from deck1` / `map a new one`；deck1 没有 unit 时 clone 会在
    watch dog 里提示
  - `add unit` 只列内置 preset，按 deck/global 过滤
  - learn 中选中 slot 行会闪烁；再次 `Enter` 确认，`Esc` 取消
  - 不提供 mode 编辑、action 编辑、selector 编辑或 encoding 切换；这些直接改 TOML

### 14-bit 的自动识别

转动/推动一个 14-bit 控件时它会连发两个 CC（高位 + 低位，编号差 32，例如 `#9` 和 `#41`）。
learn 模式下，同一个 slot 在 120ms 内收到两个相差 32 的控制器号，就记为
`endpoint = { midi = "cc", id = <MSB>, lsb = <LSB> }`，屏幕上显示成 `cc14 ch1 #9/#41`。
手动写也一样：`lsb` 就是低位，`id` 是高位。

## 10. action 表

`crates/hypermixx-core/src/action.rs::ACTIONS` 是唯一真源。

| action | 需要 deck | 说明 |
|---|---|---|
| `play` | 是 | 切换播放/暂停 |
| `cue` | 是 | 按住式 cue：按下 cue，松开归位 |
| `cue smart` | 是 | 只按一次：回 cue 或就地设 cue |
| `vinyl [frames_per_tick]` | 是 | 唱盘：press = 贴盘/松手，turn = 转轮 |
| `vinyl slip` | 是 | 下一次贴盘算 slip scratch |
| `hotcue set\|trigger\|clear <1..8>` | 是 | hot cue |
| `beatjump <±beats\|forward\|backward>` | 是 | 按整拍 seek；显式数字会更新 `jump_beats` |
| `jumpbeats double\|halve` | 是 | 改 `jump_beats`（1..64） |
| `loop <beats\|num/den>` | 是 | beat loop；正在循环且长度相同 = 退出，不同 = 改成该长度 |
| `loop toggle` | 是 | 用 `loop_beats` 开/关 loop |
| `loopbeats double\|halve` | 是 | 改 `loop_beats`；正在 loop 时同步重设 |
| `loop in` / `loop out` / `loop exit` / `loop cancel` | 是 | 手动 loop |
| `loop halve` / `loop double` | 是 | 当前 loop ÷2 / ×2 |
| `fader flow` / `deck` / `gain` / `cuesend` | 是 | 通道电平类 |
| `fader cuesend toggle` | 是 | cue send 开/关 |
| `fader cross` / `master` / `cue` | 否 | 全局电平 |
| `<stem> fader` / `mute` / `solo` | 是 | 单条 stem |
| `tempofader` | 是 | 速度推子位置 |
| `keylock [on\|off\|toggle]` | 是 | keylock |
| `key <semitone>` | 是 | 变调（占位） |
| `nudge [delta] [seconds]` | 是 | 临时速率弯折 |
| `sync tempo\|phase\|tempolock\|phaselock\|set-leader\|unlock` | 是 | 同步族 |
| `fx on\|off\|toggle\|pad\|trigger` | 否 | 按 `chain` + `fx` 寻址 |
| `fx param <name>` | 否 | 连续参数，值来自控制器 |
| `browser up\|down\|open\|exit\|load\|refresh` | 否 | 浏览器（交给前端，不进引擎） |

旧的点号拼写（`fader.flow`、`beatjump+`、`sync.leader`…）仍由
`hypermixx_core::action::canonical` 翻译到规范形式。

## 11. 加分拍长（`loop 1/4`）

`LoopOp::Beats` 收的是 `BeatLen`（分子/分母，分母是 2 的幂），所以 `1/4`、`1/2`、`3/4` 都精确：

```text
frames = frame_at_beat(floor + whole) + round(frac × beat_width(floor))
```

分数部分是**当前那拍**的比例，所以循环起点有变速也不会漂。会话里同样可以写 `loop 1/4`。
