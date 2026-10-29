> 本文描述当前代码行为；1028 的目标结构（Button / Knob / BipolKnob / RelKnob / JogWheel、1-based deck、RelKnob 双 action）见 `plans/1028unitplan.md`。

# preset / unit 速查

这份只描述**现在代码里的行为**，方便你指出要改哪一块。

---

## 1. 概念

- **preset（预置单元）**：一组固定的 slot 名 + 默认 mode 表。
  - 不包含物理端点（channel/id）。
  - 内建在代码里，用户也可以写自己的预置文件。
- **unit（实例）**：map 里真正存在的一个单元。
  - 有 `id`、`deck`、物理端点、selector，以及可选的 mode 覆盖。
  - 可以引用一个 `preset`，从而继承它的 slot 名和默认 modes。
- **slot（槽位）**：unit 里的一个角色，例如 `pad1`、`fader`、`low`、`jog`。
- **mode**：一张 `slot → action` 表。
- **selector**：只负责切换 mode 的物理键，本身不执行 action。

```text
preset:  slot 名 + 默认 mode 表
                  ↓ 被引用
unit:    deck + endpoints + selectors + mode 覆盖
                  ↓
mode:    slot → action
```

---

## 2. 文件位置

内建预置：

```text
crates/hypermixx-midi/presets/
  touchpad.toml
  mixer.toml
  transport.toml
  browser.toml
  output.toml
  jog.toml
```

用户预置：

```toml
# map 文件里
[meta]
presets = ["presets/my.toml"]
```

路径相对 map 文件所在目录。同名用户预置会覆盖内建预置。

仓库里有一个用户预置示例：

```text
profile/presets/example.toml   # preset "deckfx"
```

---

## 3. preset 的 TOML 结构

```toml
[[preset]]
name = "touchpad"                  # 必填，唯一
start = "hotcue"                   # 可选，默认第一个 mode
mode_order = ["hotcue", "loop"]    # 可选，cycle 顺序/白名单

[[preset.slot]]
slot = "pad1"
shape = "pad"                      # pad | knob | jog；可选

[[preset.mode]]
name = "hotcue"
label = "HOT CUE"                  # 可选

[[preset.mode.bind]]
slot = "pad1"
action = "hotcue trigger 1"

# bind 还支持：
# deck / min / max / curve / step / seconds / beats
# chain / fx / param
# on = "press" | "release"
```

`action` 是“去掉目标的会话命令行”，例如：

```text
play
hotcue trigger 1
beatjump 4
loop 4
fader flow
fx param
browser load
```

FX 动作需要额外字段：

```toml
[[preset.mode.bind]]
slot = "low"
action = "fx param"
fx = "eq"
param = "low"
min = -1.0
max = 1.0
```

`chain` 省略时使用 unit 的 deck（`FxChainId::Deck(deck)`），所以同一个 mixer 预置可以同时给 deck0/deck1 用。

---

## 4. 内建预置一览

| preset | slots | 默认 modes |
|---|---|---|
| `touchpad` | `pad1..pad8`，shape `pad` | `hotcue`, `loop`, `beatjump`, `stems`；start `hotcue` |
| `mixer` | `fader`, `gain`, `low`, `mid`, `high`, `filter`, `cue`, `load` | `default` |
| `transport` | `play`, `cue`, `sync`, `keylock` | `default` |
| `browser` | `up`, `down`, `open`, `exit`, `load`, `refresh` | `default` |
| `output` | `master`, `cue` | `default` |
| `jog` | `jog`，shape `jog` | `default`（`vinyl`） |
| `deckfx`（用户示例） | `knob1`, `knob2` | `filter`, `eq` |

---

## 5. unit 的 TOML 结构

```toml
[[unit]]
id = "deck0.pad"              # 全局唯一
preset = "touchpad"           # 可选
deck = 0                      # 可选；global unit 不写
start = "hotcue"              # 可选，覆盖 preset
mode_order = ["hotcue", "loop"]  # 可选，覆盖 preset

[[unit.control]]
slot = "pad1"                 # 必须是 preset 里的 slot，或 unit 自己新增的 slot
kind = "pad"                  # 可选：pad | knob | jog
type = "note"                 # cc | note | bend
mode = "abs"                  # cc 专用：abs | rel1 | rel2 | rel3
reverse = false               # 相对 CC 可选
channel = 0
id = 16

# jog 的写法：
# [[unit.control]]
# slot = "jog"
# kind = "jog"
# touch = { type = "note", channel = 0, id = 80 }
# turn  = { type = "cc", mode = "rel2", channel = 0, id = 6 }

[[unit.selector]]
type = "note"
channel = 0
id = 24
behavior = "trigger"          # trigger | cycle
target = "hotcue"             # trigger 必填；cycle 不写

[[unit.mode]]
name = "hotcue"               # 与 preset 同名 = 整个替换该 mode
[[unit.mode.bind]]
slot = "pad1"
action = "play"
on = "press"
```

---

## 6. 合并 / 编译规则

代码位置：`crates/hypermixx-midi/src/map.rs` 的 `compile_unit`。

### 6.1 slots

1. 先按 preset 的 `[[preset.slot]]` 顺序。
2. unit 自己新增的 slot 追加到后面。
3. preset 给了 shape 时，unit 的 control shape 必须一致。
4. 只有 preset slot、没有 `[[unit.control]]` 时，slot 存在但未绑定端点。

### 6.2 modes

1. 先放 preset 的 modes。
2. unit 的 `[[unit.mode]]`：
   - 同名：**整个替换** preset mode。
   - 新名：追加。
3. `mode_order`：
   - unit 写了：作为精确白名单/顺序。
   - 只有 preset 写了：按 preset 顺序，unit 新增但没列出的 mode 追加。
   - 都没写：声明顺序。
4. `start`：unit > preset > 第一个 mode。

### 6.3 selectors

- selector 只来自 unit。
- `trigger` 的 `target` 必须是最终 mode 列表里的名字。
- `cycle` 按最终顺序前进并回绕。

### 6.4 校验

- unit id 唯一；mode/slot 名不能含空白。
- 全 map 端点唯一（`type + channel + id`）。
- pad 不能驱动连续 action；knob 不能驱动 edge action。
- jog 只能驱动 `vinyl`，且 turn 必须是相对 CC。
- 两边缘 action（`cue` / `vinyl` / `fx pad` / 按住式 `nudge`）不能写 `on`。
- 连续 action 也不能写 `on`。

---

## 7. guide 现在怎么用 preset

代码位置：

- 状态机：`crates/hypermixx-midi/src/guide.rs`
- TUI：`crates/hypermixx-cli/src/tui/guide.rs`

行为：

- 启动是空 map，不自动加载文件/设备。
- `New unit`：选 preset → 选 deck → 写一个 `[[unit]] preset = ...`，然后逐个 learn endpoint。
- `Clone`：选目标 deck，深拷贝 controls/selectors/modes：
  - `channel += target_deck - old_deck`
  - 显式 `deck = old` 改新 deck
  - `chain = "deck0"` / `"d0"` / `"0"` / `"deck0/vocals"` 改写
  - `id` 前缀 `deck0.` → `deck1.`
  - `preset` 保留
- 编辑 preset mode 里的任意 bind 时，guide 会把整个 preset mode 复制成 `[[unit.mode]]` 再改，避免丢其它 slot。
- 当前 guide **不能新增 preset 之外的空 slot**；需要手改 TOML。
- FX bind 的 `chain/fx/param` 目前仍要手改；guide 的 action 编辑只改 `action` 字符串。

---

## 8. 已知限制 / 可以改的地方

1. 预置列表、slot 名、默认 mode 表。
2. 同名 mode 现在是“整个替换”，不是“合并”。
3. guide 不能新增空白 slot。
4. guide 的 FX 参数不能图形化编辑。
5. clone 目标 deck 已有同名 unit 时直接报错，需要先删。
6. preset 目前只有内建文件 + `[meta] presets`，没有 include/继承。
7. guide 左栏树会显示每个 mode 下所有 slot 的 action，mode/slot 多时列表较长。
8. 没有 LED/反馈。

---

## 9. 全部预置的 TOML（简化，去掉注释）

### 9.1 touchpad

```toml
[[preset]]
name = "touchpad"
start = "hotcue"
mode_order = ["hotcue", "loop", "beatjump", "stems"]

[[preset.slot]]
slot = "pad1"
shape = "pad"
[[preset.slot]]
slot = "pad2"
shape = "pad"
[[preset.slot]]
slot = "pad3"
shape = "pad"
[[preset.slot]]
slot = "pad4"
shape = "pad"
[[preset.slot]]
slot = "pad5"
shape = "pad"
[[preset.slot]]
slot = "pad6"
shape = "pad"
[[preset.slot]]
slot = "pad7"
shape = "pad"
[[preset.slot]]
slot = "pad8"
shape = "pad"

[[preset.mode]]
name = "hotcue"
[[preset.mode.bind]]
slot = "pad1"
action = "hotcue trigger 1"
[[preset.mode.bind]]
slot = "pad2"
action = "hotcue trigger 2"
[[preset.mode.bind]]
slot = "pad3"
action = "hotcue trigger 3"
[[preset.mode.bind]]
slot = "pad4"
action = "hotcue trigger 4"
[[preset.mode.bind]]
slot = "pad5"
action = "hotcue trigger 5"
[[preset.mode.bind]]
slot = "pad6"
action = "hotcue trigger 6"
[[preset.mode.bind]]
slot = "pad7"
action = "hotcue trigger 7"
[[preset.mode.bind]]
slot = "pad8"
action = "hotcue trigger 8"

[[preset.mode]]
name = "loop"
[[preset.mode.bind]]
slot = "pad1"
action = "loop in"
[[preset.mode.bind]]
slot = "pad2"
action = "loop out"
[[preset.mode.bind]]
slot = "pad3"
action = "loop 1"
[[preset.mode.bind]]
slot = "pad4"
action = "loop 4"
[[preset.mode.bind]]
slot = "pad5"
action = "loop 8"
[[preset.mode.bind]]
slot = "pad6"
action = "loop halve"
[[preset.mode.bind]]
slot = "pad7"
action = "loop double"
[[preset.mode.bind]]
slot = "pad8"
action = "loop exit"

[[preset.mode]]
name = "beatjump"
[[preset.mode.bind]]
slot = "pad1"
action = "beatjump -16"
[[preset.mode.bind]]
slot = "pad2"
action = "beatjump -8"
[[preset.mode.bind]]
slot = "pad3"
action = "beatjump -4"
[[preset.mode.bind]]
slot = "pad4"
action = "beatjump -1"
[[preset.mode.bind]]
slot = "pad5"
action = "beatjump 1"
[[preset.mode.bind]]
slot = "pad6"
action = "beatjump 4"
[[preset.mode.bind]]
slot = "pad7"
action = "beatjump 8"
[[preset.mode.bind]]
slot = "pad8"
action = "beatjump 16"

[[preset.mode]]
name = "stems"
[[preset.mode.bind]]
slot = "pad1"
action = "vocals mute"
[[preset.mode.bind]]
slot = "pad2"
action = "drums mute"
[[preset.mode.bind]]
slot = "pad3"
action = "bass mute"
[[preset.mode.bind]]
slot = "pad4"
action = "other mute"
[[preset.mode.bind]]
slot = "pad5"
action = "vocals solo"
[[preset.mode.bind]]
slot = "pad6"
action = "drums solo"
[[preset.mode.bind]]
slot = "pad7"
action = "bass solo"
[[preset.mode.bind]]
slot = "pad8"
action = "other solo"
```

### 9.2 mixer

```toml
[[preset]]
name = "mixer"
start = "default"

[[preset.slot]]
slot = "fader"
shape = "knob"
[[preset.slot]]
slot = "gain"
shape = "knob"
[[preset.slot]]
slot = "low"
shape = "knob"
[[preset.slot]]
slot = "mid"
shape = "knob"
[[preset.slot]]
slot = "high"
shape = "knob"
[[preset.slot]]
slot = "filter"
shape = "knob"
[[preset.slot]]
slot = "cue"
shape = "pad"
[[preset.slot]]
slot = "load"
shape = "pad"

[[preset.mode]]
name = "default"
[[preset.mode.bind]]
slot = "fader"
action = "fader flow"
[[preset.mode.bind]]
slot = "gain"
action = "fader gain"
[[preset.mode.bind]]
slot = "low"
action = "fx param"
fx = "eq"
param = "low"
min = -1.0
max = 1.0
[[preset.mode.bind]]
slot = "mid"
action = "fx param"
fx = "eq"
param = "mid"
min = -1.0
max = 1.0
[[preset.mode.bind]]
slot = "high"
action = "fx param"
fx = "eq"
param = "high"
min = -1.0
max = 1.0
[[preset.mode.bind]]
slot = "filter"
action = "fx param"
fx = "filter"
param = "value"
[[preset.mode.bind]]
slot = "cue"
action = "fader cuesend toggle"
[[preset.mode.bind]]
slot = "load"
action = "browser load"
```

### 9.3 transport

```toml
[[preset]]
name = "transport"
start = "default"

[[preset.slot]]
slot = "play"
shape = "pad"
[[preset.slot]]
slot = "cue"
shape = "pad"
[[preset.slot]]
slot = "sync"
shape = "pad"
[[preset.slot]]
slot = "keylock"
shape = "pad"

[[preset.mode]]
name = "default"
[[preset.mode.bind]]
slot = "play"
action = "play"
[[preset.mode.bind]]
slot = "cue"
action = "cue"
[[preset.mode.bind]]
slot = "sync"
action = "sync phase"
[[preset.mode.bind]]
slot = "keylock"
action = "keylock toggle"
```

### 9.4 browser

```toml
[[preset]]
name = "browser"
start = "default"

[[preset.slot]]
slot = "up"
shape = "pad"
[[preset.slot]]
slot = "down"
shape = "pad"
[[preset.slot]]
slot = "open"
shape = "pad"
[[preset.slot]]
slot = "exit"
shape = "pad"
[[preset.slot]]
slot = "load"
shape = "pad"
[[preset.slot]]
slot = "refresh"
shape = "pad"

[[preset.mode]]
name = "default"
[[preset.mode.bind]]
slot = "up"
action = "browser up"
[[preset.mode.bind]]
slot = "down"
action = "browser down"
[[preset.mode.bind]]
slot = "open"
action = "browser open"
[[preset.mode.bind]]
slot = "exit"
action = "browser exit"
[[preset.mode.bind]]
slot = "load"
action = "browser load"
[[preset.mode.bind]]
slot = "refresh"
action = "browser refresh"
```

### 9.5 output

```toml
[[preset]]
name = "output"
start = "default"

[[preset.slot]]
slot = "master"
shape = "knob"
[[preset.slot]]
slot = "cue"
shape = "knob"

[[preset.mode]]
name = "default"
[[preset.mode.bind]]
slot = "master"
action = "fader master"
[[preset.mode.bind]]
slot = "cue"
action = "fader cue"
```

### 9.6 jog

```toml
[[preset]]
name = "jog"
start = "default"

[[preset.slot]]
slot = "jog"
shape = "jog"

[[preset.mode]]
name = "default"
[[preset.mode.bind]]
slot = "jog"
action = "vinyl"
```

### 9.7 用户示例 deckfx

```toml
[[preset]]
name = "deckfx"
start = "filter"

[[preset.slot]]
slot = "knob1"
shape = "knob"
[[preset.slot]]
slot = "knob2"
shape = "knob"

[[preset.mode]]
name = "filter"
[[preset.mode.bind]]
slot = "knob1"
action = "fx param"
fx = "filter"
param = "value"
[[preset.mode.bind]]
slot = "knob2"
action = "fx param"
fx = "filter"
param = "resonance"

[[preset.mode]]
name = "eq"
[[preset.mode.bind]]
slot = "knob1"
action = "fx param"
fx = "eq"
param = "low"
min = -1.0
max = 1.0
[[preset.mode.bind]]
slot = "knob2"
action = "fx param"
fx = "eq"
param = "high"
min = -1.0
max = 1.0
```

---

## 10. 待确认的 MIDI 行为

### 10.1 tempofader 每次送两个 CC

先看 raw monitor，分三种情况：

1. **两个不同 CC 号，通常是 n 和 n+32**：14-bit CC（MSB/LSB）。
   - 建议新增 `type = "cc14"`：
     ```toml
     [[unit.control]]
     slot = "fader"
     type = "cc14"
     channel = 0
     id = 0        # MSB；LSB 默认 id + 32
     # lsb = 32   # 可选覆盖
     ```
   - 组合成 `0..16383` 再归一化；只在 LSB 到达后发一次，避免每推一下发两个值。
2. **两个消息是同一个 CC 号**：可能是重复发送/平滑。
   - 加一个 per-endpoint 的 `dedupe_ms` 或“成对取后一个”的选项，不要当成两次移动。
3. **两个不同 CC 号，但都是 0/127 之类**：可能是 up/down 两个键。
   - 用 `type = "cc_pair"`，分别 `up`/`down`，每次 127 产生 +1/−1 的相对 tick。

### 10.2 browser 旋钮 / jog 的“两点 CC”

如果是**同一个 CC**，只出现 `0/1/127` 这类少数值：

- 用 `mode = "rel2"`，不要用 `rel1`。
  - `rel2`：1..63 = +，65..127 = −，0 = 不动。
  - `rel1` 会把 1 解释成 −63、127 解释成 +63，表现为巨大跳变。
- 当前 `rel3` 对 `0` 的处理是错的（0 会被当成 +1）；如果设备用 `0` 表示中立，应修 `rel3` 或直接用 `rel2`。
- guide 的自动检测目前看到 `1 → 127` 会判成 `rel1`，这对两点编码器是错的；建议：
  - 在 learn 时如果检测到值只在 `{0,1,127}` 附近，默认 `rel2`；
  - 或者不自动判，直接在 slot 上提供 `rel2` 快捷按钮。

如果是**两个不同 CC 号**（一个向上、一个向下），用 `cc_pair` 或两个 endpoint 合成一个 slot。

### 10.3 建议的实现顺序

1. 先加 `cc14`（tempofader 最可能是这个）。
2. 修 `rel3` 的 0 中立值，并把 guide 自动检测从 `rel1` 改为对两点值默认 `rel2`。
3. 需要时再加 `cc_pair`。
4. `dedupe_ms` 只作为兜底，不要默认开启。

