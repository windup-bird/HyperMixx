# MIDI 映射文件参考

一个映射文件把「MIDI 事件」翻成引擎的 `Command`。schema 与校验在
`crates/hypermixx-midi/src/map.rs`，`Event → Command` 的翻译在 `translate.rs`，learn 模式编辑器在
`hypermixx_midi::guide` + `crates/hypermixx-cli/src/tui/guide.rs`。

> 会话命令的语法见 [`commands.md`](commands.md)。本文只讲映射文件本身。
> action 名与 CLI 语法的统一（一张语法表）是后续工作；下面描述的是当前版本。

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
| `action` | string | 见 §5 |
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

`crates/hypermixx-midi/src/map.rs::ACTIONS` 是唯一真源，`--midi-guide` 的目标清单由它生成。
`deck` 列表示该 action 必须给 `deck` 字段。

| action | deck | 说明 |
|---|---|---|
| `play` | 是 | 切换播放/暂停 |
| `cue` | 是 | 按住式 cue 键（note）：按下 = `Hold`，松开 = `Back`。唯一的 cue 行 |
| `cue.smart` | 是 | 一次性 cue（不出现在 guide 清单里；给只想「记 cue 点」的按钮用） |
| `vinyl` | 是 | 唱盘：note 绑定 = 贴盘/松手，`cc` 相对绑定 = 转轮（`step` = 每 tick 帧数，缺省 1323） |
| `beatjump+` / `beatjump-` | 是 | 按 `step` 拍（缺省 1 拍）前后跳 |
| `fader.flow` | 是 | 通道级电平（装了 stems 就写全部四路） |
| `fader.stem.<stem>` | 是 | 单路 stem 电平（`<stem>` = drums \| bass \| other \| vocals） |
| `stem.<stem>.mute` | 是 | latch 该 stem 的静音 |
| `stem.<stem>.solo` | 是 | latch 该 stem 的 solo 成员资格 |
| `fader.deck` | 是 | deck 电平 |
| `fader.gain` | 是 | 通道 trim（`0` = 0 dB，`1` = +6 dB，`-1` = 静音） |
| `fader.cuesend` | 是 | cue 送出量（线性 0..=1） |
| `fader.cross` | 否 | crossfader |
| `fader.master` | 否 | master trim |
| `fader.cue` | 否 | cue trim |
| `tempofader` | 是 | 速度推子位置（`-1..=1`，按 deck 自己的 `temporange` 折算） |
| `nudge` | 是 | 临时速率弯折：`step` = delta（缺省 0.15），`seconds` 给了 = 定时，不给 = 按住 |
| `loop.in` / `loop.out` | 是 | 手动 loop 入/出点 |
| `loop.exit` / `loop.cancel` | 是 | 退出 loop / 取消armed in |
| `loop.halve` / `loop.double` | 是 | 当前 loop 长度 ÷2 / ×2 |
| `loop.beat` | 是 | beat loop，拍数取 `beats` 字段，或 `loop.beat4` 这种后缀写法 |
| `sync.tempo` | 是 | 对 BPM 一次 |
| `sync.phase` | 是 | 对拍（含 tempo） |
| `sync.tempolock` | 是 | 共享 tempo |
| `sync.phaselock` | 是 | follower 严格跟随 leader |
| `sync.leader` | 是 | 把该 deck 设为 leader |
| `sync.unlock` | 是 | 解锁并清修正 |
| `fx.on` / `fx.off` / `fx.toggle` | 否 | 按 `chain` + `fx` 开关一个槽位 |
| `fx.pad` | 否 | 按住 = engaged，松开恢复原状态 |
| `fx.trigger` | 否 | 触发一次性动作 |
| `fx.param` | 否 | 写 `chain` + `fx` + `param` |

## 6. FX 的寻址

文件里 FX 只写**名字**（`chain`/`fx`/`param`），从不写槽位号：槽位会因增删移位，文件必须能跨重启。
CLI 启动时用 `ListFx` 拿到每条链的槽位名，再 `Map::resolve_fx` 把名字钉到索引上。

## 7. 打开与编辑

- `--midi <port> [--midi-map <file>]`：启动时打开端口并按映射翻译（缺省 `midi-map.toml`）。
  端口可以是名字或序号（`midi ports` 可列）。
- `--tui` 下 `F2` 选端口、`F3` 选映射文件。
- `--midi-guide [<file>] [--midi <port>] [--decks <n>]`：learn 模式编辑器，**不启动音频引擎**。
  键位 `↑↓ Enter 空格 m x s p f q`；右侧 raw 监视器是判断 `mode` 与数 tick 的地方。
