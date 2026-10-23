# MIDI 映射文件参考

一个映射文件把「MIDI 事件」翻成引擎的 `Command`。schema 与校验在
`crates/hypermixx-midi/src/map.rs`，`Event → Command` 的翻译在 `translate.rs`，learn 模式编辑器在
`hypermixx_midi::guide` + `crates/hypermixx-cli/src/tui/guide.rs`。

> 会话命令的语法见 [`commands.md`](commands.md)。本文只讲映射文件本身。
>
> **`action` 就是一条去掉目标的会话命令**：`"play"`、`"beatjump 4"`、`"sync phase pid"`、
> `"vocals mute"`、`"fader gain"`。目标由 binding 的 `deck` 字段提供；行首自己写了目标词
> （`"d1 play"`、`"master fx toggle filter"`）则以那个为准。语法表在
> `crates/hypermixx-core/src/action.rs::ACTIONS`，guide 的目标清单、这份文档与全部 action 都由它生成。

---

## 1. 文件骨架

```toml
[meta]
name = "my-controller"          # 可选，仅用于标识

[[bind]]
type = "note"                   # cc | note | bend
channel = 0                     # 0-based；省略 = 匹配任意通道
id = 0                          # CC 号 / note key（bend 忽略）
deck = 0                        # 目标 deck；省略 = 全局目标
action = "play"
```

- 解析是 `deny_unknown_fields`：拼错的键是**报错 + TOML 行号**，不是静默忽略。
- 一份 bind 匹配一个 `(type, channel, id)`；`channel` 省略时匹配任意通道。
- 同一个事件可以被多条 bind 匹配（互不排斥），但通常一条控件一条 bind。

## 2. 每个 bind 的字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `type` | `cc` \| `note` \| `bend` | 监听的事件种类。必填 |
| `mode` | `abs` \| `rel1` \| `rel2` \| `rel3` | 仅 `cc`；相对编码器的回绕约定，见 §3 |
| `reverse` | bool | 相对模式的**反向**开关（`rel1`/`rel2`/`rel3`）：整体取反 |
| `channel` | 0..=15 | 省略 = 任意通道 |
| `id` | u8 | CC 控制器号或 note key |
| `deck` | u8 | 目标 deck；需要 deck 的 action 必须给 |
| `action` | string | 一条去掉目标的会话命令，见 §5（旧的点号拼写仍被接受，见 §5.1） |
| `min` / `max` | f32 | 归一化到 `0..=1` 后的映射端点（推子默认按其域：双极 `-1..=1`，FX 参数 `0..=1`） |
| `curve` | `linear` \| `sharp` | 端点之间的响应曲线（`sharp` = 平方律，底部更细） |
| `step` | f32 | 连续动作的步长/量（`beatjump` 的拍数、`vinyl` 每 tick 帧数、`nudge` 的 delta） |
| `seconds` | f64 | `nudge` 的持续秒数；给了就是定时释放（忽略松开边沿），不给 = 按住式 |
| `beats` | u64 | `loop.beat` 的拍数 |
| `chain` | string | FX 目标链：`master`/`m`、`deck0`/`d0`/`0` |
| `fx` | string | 效果名（不是槽位号 —— 槽位会因增删而移位，名字不会） |
| `param` | string | 参数名（`fx.param` 用） |

## 3. 相对编码器（`mode`）

厂商约定不同，所以 `mode` 是**每条 bind** 的属性。`relative_delta`（`translate.rs`）的解码规则：

| mode | 规则 |
|---|---|
| `rel1` | 以 64 为中心的偏移二进制：`value − 64`，64 = 不动，65..127 = +，1..63 = − |
| `rel2` | 以 64 为中心的二进制补码：1..63 = +，65..127 = −（127 = −1） |
| `rel3` | 只看方向：1..63 = +，65..127 = −，幅度忽略（每条消息算一步） |
| `abs` | 绝对：`value/127` 归一化后按 `min`/`max`/`curve` 映射 |

**怎么判断自己的设备是哪种**：打开 `--midi-guide` 的 raw 监视器，慢转一圈看取值：
63/64/65 附近 = `rel1`；1,2,3 / 127,126 = `rel2`（或 `rel3`，如果幅度不变化）。

`reverse = true` 把解出的 delta 取反。相对编码器的**方向自动识别**留到 guide 重构时再做，
当前需要手动试。

## 4. 值的整形

绝对控件先归一化到 `0..=1`，再经 `curve`、再线性映到 `min..max`：

```
norm   = value / 127
shaped = curve(norm)          # linear: norm；sharp: norm²（双极动作在中心折返，中心点不动）
value  = min + shaped × (max − min)
```

- 推子（`fader.*`、`tempofader`）是**双极**的：默认 `min = -1`、`max = 1`，0 = unity。
- `fader.cuesend` 是线性的 `0..=1`。
- FX 参数默认 `0..=1`（效果自己的域）。

## 5. action 表

`crates/hypermixx-core/src/action.rs::ACTIONS` 是唯一真源：guide 的目标清单、这份文档、以及两侧的
语法都由它生成。`deck` 列表示该 action 必须给 `deck` 字段（除非行首自己写了目标）。

| action（= 会话命令，去掉目标） | deck | 说明 |
|---|---|---|
| `play` | 是 | 切换播放/暂停 |
| `cue` | 是 | 按住式 cue 键（note）：按下 = cue，松开 = 归位 |
| `vinyl [frames_per_tick]` | 是 | 唱盘：note = 贴盘/松手，相对 CC = 转轮（缺省 1323 帧/tick） |
| `beatjump [beats]` | 是 | 按整拍 seek，保持相位（可正可负；缺省量取 `step`） |
| `hotcue set\|trigger\|clear <slot>` | 是 | 1..8（A..H）的 hot cue；trigger 保持当前播放状态 |
| `fader flow` | 是 | 通道级电平（装了 stems 就写全部四路） |
| `<stem> fader` | 是 | 单路 stem 电平（`<stem>` = drums \| bass \| other \| vocals） |
| `<stem> mute` | 是 | latch 该 stem 的静音 |
| `<stem> solo` | 是 | latch 该 stem 的 solo 成员资格 |
| `fader deck` | 是 | deck 电平 |
| `fader gain` | 是 | 通道 trim（`0` = 0 dB，`1` = +6 dB，`-1` = 静音） |
| `fader cuesend` | 是 | cue 送出量（线性 0..=1） |
| `fader cross` | 否 | crossfader |
| `fader master` | 否 | master trim |
| `fader cue` | 否 | cue trim |
| `tempofader` | 是 | 速度推子位置（按 deck 自己的 `temporange` 折算） |
| `nudge [delta] [seconds]` | 是 | 临时速率弯折；给了秒数就是定时释放，否则按住式 |
| `loop in` / `loop out` | 是 | 手动 loop 入/出点 |
| `loop exit` / `loop cancel` | 是 | 退出 loop / 取消 armed in |
| `loop halve` / `loop double` | 是 | 当前 loop 长度 ÷2 / ×2 |
| `loop [beats]` | 是 | beat loop（缺省 4 拍；也可用 binding 的 `beats` 字段） |
| `sync tempo` | 是 | 对 BPM 一次 |
| `sync phase [mode] [seconds]` | 是 | 对拍（含 tempo），mode = instant \| linear \| pid（缺省 pid） |
| `sync tempolock` | 是 | 共享 tempo |
| `sync phaselock [mode] [seconds]` | 是 | follower 严格跟随 leader |
| `sync set-leader` | 是 | 把该 deck 设为 leader |
| `sync unlock` | 是 | 解锁并清修正 |
| `fx on` / `fx off` / `fx toggle` | 否 | 按 `chain` + `fx` 开关一个槽位 |
| `fx pad` | 否 | 按住 = engaged，松开恢复原状态 |
| `fx trigger` | 否 | 触发一次性动作 |
| `fx param` | 否 | 写 `chain` + `fx` + `param`（这是 `fx set` 的连续形式：值来自控制器） |
| `browser up` / `browser down` | 否 | 浏览器选择上/下 |
| `browser open` / `browser exit` | 否 | 进入下一级/进入 tracks；返回上一级 |
| `browser load` | 否 | 加载当前 track；binding 的 `deck` 可选 |
| `browser refresh` | 否 | 重新扫描所有曲库来源 |

`browser *` 不会进入引擎：`translate_split` 把它们交给前端（TUI 的浏览器），其余 action 仍走
`Command`。REPL 没有浏览器状态，因此忽略这些动作。

`fx toggle` 与其他 FX 动作一样由**引擎**翻状态（`Command::ToggleFx`），前端不留镜像 —— 否则任何别的
路径改了槽位状态，镜像就错了。

### 5.1 旧拼写

点号拼写（`fader.flow`、`beatjump+`、`stem.vocals.mute`、`sync.leader`、`loop.beat8`…）**仍然可用**，
由 `hypermixx_core::action::canonical` 翻译到规范形式；guide 保存时会写成新形式。文件是用户手编的，
升级后静默失效比多认几个拼写糟得多。

## 6. FX 的寻址

文件里 FX 只写**名字**（`chain`/`fx`/`param`），从不写槽位号：槽位会因增删移位，文件必须能跨重启。
CLI 启动时用 `ListFx` 拿到每条链的槽位名，再 `Map::resolve_fx` 把名字钉到索引上。

## 7. 打开与编辑

- `--midi <port> [--midi-map <file>]`：启动时打开端口并按映射翻译（缺省 `midi-map.toml`）。
  端口可以是名字或序号（`midi ports` 可列）。
- `--tui` 下 `F2` 选端口、`F3` 选映射文件。
- `--midi-guide [<file>] [--midi <port>] [--decks <n>]`：learn 模式编辑器，**不启动音频引擎**。
  键位 `↑↓ Enter 空格 m x s p f q`；右侧 raw 监视器是判断 `mode` 与数 tick 的地方。
