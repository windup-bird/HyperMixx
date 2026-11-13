# Hypermixx 命令参考

TUI（默认）与 `--cli` 的 REPL 共用同一套语法（`crates/hypermixx-cli/src/command.rs`）与同一套响应渲染
（`response.rs`），本文档覆盖：启动参数、会话语法、全部会话命令、TUI 键位与输出格式。
控制器映射文件的 action 表见 [`MIDI.md`](MIDI.md)。

---

## 1. 启动参数

```bash
cargo run -p hypermixx-cli -- [flags]
```

| 参数 | 说明 |
|---|---|
| `--cli` | 行式 REPL；不给则启动 TUI（默认） |
| `--config <file>` | 指定配置文件（单文件多段落）；不给则用平台默认路径，首次启动从内置模板复制一份。`HYPERMIXX_CONFIG` 可覆盖路径 |
| `--guide [<file>]` | learn 模式地图编辑器（**不启动音频引擎**）；缺省编辑 config 同目录的 `mapping.toml`，不存在则从内置模板创建 |
| `--4-decks` | 四碟界面：引擎起四条 deck，TUI 的 deck 区按页纵向堆叠，控制带每列各自翻页。不给就是双碟 |

- 未知参数是硬错误（`error: unknown argument …`），不会静默忽略；
  `--profile` / `--live` / `--controller-map` / `--tui` / `--midi*` / `--backend` 已移除。
  离线分析后端只由配置决定：`[preference.analysis]` 的 `beats` / `key`（`stratum` | `timestretch`），
  没有启动参数、也没有 `auto` 回落。
- 控制器只由配置文件描述（`[[controller]]` 的 `device`/`map`），没有命令行参数；`--guide` 打开
  地图后按 `Connect` 选端口，失败会留在 watch dog 里等下一次。
- deck 数量由 `--4-decks` 决定（缺省 2）；配置里的 `[[multideck.deck]]` 只提供
  `<controller>.<face>[.mirror]` 映射，不决定有几个 deck。

---

## 2. 会话语法

### 2.1 target-first：行首选目标

```
[deckN | dN | N | master | m] <verb> [args…]
```

- 行首是 deck 词则覆盖目标，否则动词作用于**焦点 deck**（TUI 里 `Tab` 切换焦点，
  命令框前缀随之变化）。
- `deck0` / `d0` / `0` 等价；`master` / `m` 指总线。
- **`master` 只接受 `fx` 族命令**，其余动词报 `` `xxx` needs a deck, not `master` ``。
- deck 编号超出实际通道数时该词不被识别为目标，会落成“未知命令”错误。
- 这只是前端语法，引擎的 `Command` 协议不受影响。

### 2.2 两类错误

| 来源 | 时机 | 表现 |
|---|---|---|
| 前端解析错误 | 命令发出前 | 立即 `error: usage: …`（`Failed`） |
| 引擎拒绝 | 下个块边界回包 | `error: …`（`CommandResponse::Error`），如无网格的 sync、反向锁 |
| 成功 | — | `Ok` 渲染为空，**不回显**（高频命令不刷屏） |

### 2.3 时间单位

- 帧 = 1/引擎采样率 秒（缺省 44.1 kHz 立体声；`profile/config.toml` 的 `[audio] sample_rate` 可改）；
  `jump` 用帧，`beatjump`/`loop`/`sync` 用拍。

---

## 3. 会话命令

### 3.1 载入与分析

```
[deck] load <path> [bpm]    后台解码装载；bpm > 0 则建常网格、跳过分析
[deck] analyse              后台跑分析器并发布网格（analyze 同义）
[deck] hotcue set|trigger|clear <1..8>
                            设置 / 跳到 / 清除一个 hot cue（A..H）；trigger 保持播放/暂停状态
```

从曲库加载时，导入的 beatgrid 和 memory/hot cue 会随后一起写入 deck；`hotcue` 可以继续改它们。

- `load` 的 `bpm` 非数字 → 前端直接报 `bpm must be a number`。
- TUI 里 `load` **不带路径**会弹文件浏览器（支持 mp3/wav/flac/ogg/m4a 等）。
- `analyse` 只用配置 `[preference.analysis]` 指定的后端（`beats` / `key`），没有命令行覆盖；完成提示形如
  `[analyse] deck0: 122.0 BPM, 8A`。
- 装载是后台线程，立即回 `[load] deck0: decoding …`；完成回
  `deck0 loaded: 18462369 frames (7:00.648)`。

### 3.1b 曲库

```text
browser refresh [source-id]           增量重扫已配置来源（TUI 只扫当前配置）
browser list                          来源 id、根目录、根 list 数、上次扫描时间
browser tracks <list-id>              list 里的 track（方括号是 location id）
browser show <location-id>            title / artist / album / BPM / key / 路径
browser up|down|open|exit|load         TUI 浏览器导航/加载
browser analyse                       分析当前选中的 track
```

来源由 `config.toml` 的 `[library]` 决定：

```toml
[library]
local = ["rkb", "serato", "vdj"]
usb = ["rkb", "serato", "vdj"]
folders = ["~/Music"]
```

`~` 在 `folders` 里展开为用户主目录。启动时 browser 优先打开 `~/Music`；没有这个来源时
停在第一个已配置来源。扫描在后台进行，界面先显示缓存，完成后自动刷新。未配置的类别不扫描、不显示；USB 自动发现并挂载，同一个 U 盘的多种导出是多个 library。

- 数据库在 `$HYPERMIXX_CACHE/library/library.sqlite3`（默认 `.cache/library/`），是**可重建缓存**。
- U 盘只读：rekordbox 的 `exportLibrary.db` 会先复制 `.db/-wal/-shm` 再打开；没有它时回退
  `export.pdb`。Serato 从 `database V2` + crate + 音频 ID3 `GEOB` 读取。
- 加载时优先使用导入 grid，其次本地 `analyse` 缓存，最后才用标签 BPM。
- 导入网格会减去解码器 gapless 起点（MP3 样本 25.0567ms），再应用 `grid_offset_ms` 微调；
  RB / Serato / VirtualDJ 的来源偏移和这两个值都在 `[library]` 配置；样本 USB 的默认是
  RB 0、Serato -16.5、VDJ 0。
- key 默认 Camelot；`config.toml` 的 `[display] key = "camelot" | "classic"` 切换。
- TUI 里 `Tab` 到 browser：`↑/↓` 选择，`→/Enter` 打开（最低一级进入 tracks），`←/Esc` 返回；
  `load [deck]` 加载，`analyse` 分析选中 track；鼠标单击选择，双击 track 弹 deck0/deck1 选择。
- 默认 rekordbox 口令是公开常量；覆盖用 `HYPERMIXX_REKORDBOX_KEY`。

### 3.2 走带

```
[deck] play                  切换播放/暂停（toggle；`pause` 已移除）
[deck] cue [play|back|set|smart]
                             cue 点：`play` 从 cue 播放，`back` 回 cue 并暂停，`set` 把当前帧
                             记为 cue 点，`smart` = 引擎按走带决定（播放中→back，暂停中→set）
                             **bare `cue` = 整颗 cue 键**：第一次是按下、再一次是松开
                             （状态在引擎里，所以前端不需要自己记走带）
[deck] vinyl [touch|release|slip|turn <ticks>]
                             唱盘：`touch` = 手放上去（暂停并记住走带），`turn` = 转轮（手持时
                             按 tick 移动播放头，空手时是 pitch bend），`release` = 松手（恢复
                             被 touch 打断的走带）。bare = touch
[deck] jump <frame>         按帧 seek（1 秒 = 引擎采样率帧）
[deck] beatjump <beats|forward|backward>
                            按整拍 seek；显式数字同时更新 jump_beats（缺省 8）
[deck] jumpbeats double|halve  改 jump_beats（1..64）
[deck] tempo <ratio>        设 tempo 绝对值（1.0 = 原速，0.25..4.0）
[deck] tempofader <-1..1>   推子位置，tempo = 1 + pos × temporange（MIDI 走这条）
[deck] temporange <0..1>    推子满量程（缺省 0.1 = ±10%）；只改映射，不动当前 tempo
[deck] keylock on|off       变调引擎（缺省 on；off = tape 零延迟直通；窗口见 config.toml 的 [keylock]）
[deck] key <semitones>      变调（**占位**：只记录并回读，暂不发声）
[deck] fader <stage> <pos>  电平：`flow`/`deck`/`gain`/`cuesend` 作用在目标 deck，
                            `cross`/`master`/`cue` 是共用控制（忽略目标）
                            pos 为 `-1..1`（0 = unity），`cuesend` 是线性的 `0..1`
output list|refresh         目的地与设备绑定（见 3.9）
mixer reload [<file>]       热加载拓扑（见 3.8）
live show|reload [<file>]   本机设置：引擎率/ring/stem 默认值（见 3.11）
state                       打印全部 deck 的状态行
```

- `cue` 点默认在装载原点（帧 0）；`Load` 换 deck 时回到 0。`back`/`play`/`hold` 走非阻塞
  jump，落地可能滞后一个块 + 预热时间（`hold` 在**暂停**那一路不跳，所以是即时的）。
- **`vinyl` 走 Tape 风格的 signed-rate 读取器**（cubic、零 FFT、支持反向）；`vinyl slip` 让下一次 touch
  成为 slip vinyl：`playing` 继续跑，signed-rate scratch 出声。
- **`vinyl`（唱盘）**：一颗转盘在真实设备上是两个控件 —— 一个 note（贴盘传感器）+ 一个相对
  CC（轮子本身）。两个绑定都写 `action = "vinyl"`，**转一下到底什么意思由引擎判定**（只有它
  知道此刻手在不在盘上）：
  - **手在盘上** → 暂停 + 每块把播放头**钉到**手搓到的位置（每 tick `frames_per_tick` 帧；
    `frames_per_tick = 1.8s × 采样率 / 轮子每圈 tick 数`。`jog` preset 按 340 tick/圈用 234；
    裸 `vinyl` 的兜底默认是 1323 = 60 tick/圈，建议在映射里写明实际值）。**只有轮子动的那一块出声**：移动可闻、静止静音 —— 真唱盘静止时也没有信号（而不是
    把同一个颗粒反复播成嗡嗡声）。**暂停中的 deck 一样能搓**（照样出声），松手后仍是暂停。
    反向能搓（引擎只前进，位置由 deck 自己记账，所以后退不会被"格内前进"抵消）。
  - **手不在盘上** → pitch bend：每条消息**重设**偏移并重新计时，不是无限累加。弯折的强度
    与时长由映射里 bind 的 `bend` 字段决定（一条 `bend <delta> [seconds]`，写法见
    [`MIDI.md`](MIDI.md)），`jog` preset 默认写 0.05 / 0.05；不写 `bend` 时用引擎缺省
    0.15 / 0.06。没有额外的音乐上限（最终播放率仍受引擎 `tempo+nudgerate` 的安全范围约束）。
  - 松手恢复"被 touch 打断的那条走带"（本来在放 → 继续放，本来暂停 → 保持暂停）。
  - **保险**：任何显式 `play`/`pause`/`toggle` 都会先**松盘**，丢一个 note-off（拔线、控制器
    丢包）不会让 deck 永远卡在暂停里。
- **按住式 cue 按钮**（MIDI note）是一对边沿：按下 `Hold`、松开 `Back`。`Hold` 由引擎按走带
  自己判定：
  - **播放中** → back cue（跳回 cue 点并暂停）。走带本来就在跑，所以不叠加"从 cue 播放"
  - **暂停中** → 当前播放头成为 cue 点，并**开始播放**（找位置和试听是一个动作）
  - 松开 → 无论哪种情况都回到 cue 点并暂停
- **bare `cue`（CLI/TUI）就是同一颗键**：引擎记着它是否处于"按下"，所以敲一次是按下、再敲一次
  是松开 —— 键盘也能做出"按住试听、松开归位"的手势。任何显式 op（`play`/`back`/`set`/`smart`）
  都会把该状态清回"松开"。
- **tempo / tempofader / temporange**：`tempo` 是绝对值（sync、脚本用）；`tempofader` 是物理推子
  位置（MIDI 用），引擎按 `1 + pos×range` 换算并走同一套 sync 分支。`temporange` **只改映射**，
  当前 tempo 不动（推子的反推位置随之变化）；范围 `(0, 1.0]`，缺省 0.1。
  `state` 里的 `bpm_at_frame × tempo` 就是「当前 BPM」。
- `keylock` 切换是重建（seek 价），不是实时 morph；默认 `on` 带 ≈12.7ms 管线延迟。

### 3.3 拍同步 `sync`

```
[deck] sync tempo                        一次性对 BPM（写自己的 tempo）
[deck] sync phase [instant]               一次性：先对 BPM，再直接落到 leader 相位；锁定时再触发 = 解锁
[deck] sync tempolock                    双向共享一个 tempo，任一边 fader 都带动对方（别名 lock）
[deck] sync phaselock [linear [seconds]|pid]
                                          单向持续：follower 精确跟随 leader（其 fader 被忽略），缺省 pid
[deck] sync set-leader                   宣布“本 deck 是 leader”，无参数（别名 leader）
[deck] sync unlock                       解锁：清 lock/align/nudgerate，tempo 保留（别名 off）
```

相位模式 `<mode>`：

| 模式 | 别名 | 行为 |
|---|---|---|
| `instant` | `jump` | 一次换流直接落点，不留控制器 |
| `linear [秒]` | `lin` | 定斜率 `err/t` 收敛（缺省 2.0s）；**只有 linear 接受秒数**，其余模式给秒数会前端报错 |
| `pid` | `pi` / `pll` | PI 控制器（kp 1.0 / ki 0.2，输出 ±5% 限幅兜底） |

规则与拒绝（引擎回 `Error`）：

- 两 deck 互相同步会拒绝第二条；leader（显式或自动）拒绝自同步；**leader 必须有网格**。
- **自动 leader**：没有 `set-leader` 时，先开始播放且有网格的 deck 自动成为 leader；leader
  播放期间不会因为别的 deck 更响而换人；leader 停播后，在仍在播放且有网格的 deck 里选当前
  输出电平（`Channel::peak()` 的 200ms 平滑）最高的，平局按起播顺序。锁定期间 leader 冻结；
  显式 `set-leader` 永远优先，同步别的 deck 到它也不会把显式降级成自动。
- `sync tempo` 在 `tempolock`/`phaselock` 锁下直接拒绝（写了也会被 `group_bpm` 盖回）；
  `sync phase` 在锁下不重复对拍，而是**解锁整组**；`sync phaselock` 才是持续相位控制器。
- `sync unlock` 解的是**整组**（`[deck_id, leader, follower]` 一起清），但 `tempo` 与
  `leader` 都保留。
- 实测收敛特性见 README 的 `sync_phase.sh` 表格。

### 3.4 手动微调 `bend`

```
[deck] bend <delta> <seconds>    临时速率弯折（0.04 = +4%），到点自动松开；**秒数必填**
[deck] bend off                  立即松开（ramp 回零，不咔哒；别名 stop/release/reset）
```

只动 `nudgerate`，结束时 `tempo` 原封不动。bend 没有独立的强度上限；同步的 PID 控制器仍保留
自己的 ±5% 收敛兜底。
MIDI 的按住弯折用 momentary `bend` binding；要定时就把 `seconds` 写进 mapping（见
[`MIDI.md`](MIDI.md)）。旧拼写 `nudge` 仍被接受，会自动归到 `bend`。

### 3.5 循环 `loop`

```
[deck] loop in                    武装 in 点（按拍量化），后台准备 looper
[deck] loop out                   按量化后的 out 点硬切入
[deck] loop <beats>               N 拍循环；正在循环且长度相同 = 退出，不同 = 改成该长度
[deck] loop toggle                用 loop_beats 开/关循环
[deck] loopbeats double|halve     改 loop_beats（1/32..64）；循环中同步重设
[deck] loop slip in               武装 slip loop：playing 继续跑，shadow 出声
[deck] loop slip out              切入 slip loop
[deck] loop slip <beats>          N 拍 slip loop（playing 不丢弃）
[deck] loop slip roll <beats>     N 拍反向 slip roll
[deck] loop exit                  退出普通/slip loop；slip 退出回到 playing 的 slip 位置
[deck] loop cancel                丢弃已武装的 in
[deck] loop halve | double        当前长度 ÷2 / ×2，域 1/32..64 拍（别名 /2 ÷2 / x2 *2 ×2）
[deck] loop edit len <beats>      设长度（保留 in，可小数到一个 quantum）
[deck] loop edit move <beats>     整段平移（in/out 同移，整拍）
[deck] loop edit in <beats> | out <beats>   只动其中一端（别名 length|size / shift）
[deck] loop quantum <q>           out 点量化粒度：beat | half | quarter | eighth
                                  （别名 b 1 / 1/2 2 / 1/4 4 / 1/8 8；缺省 beat）
```

- 循环中编辑 = 整段 Range 一次原子 store，不换 voice、无间隙。
- slip 模式不丢弃 `playing`：退出只丢 `shadow`，`playing` 已经在 slip 位置。
- `state` 行会显示 `loop [in-out]`、`in armed@帧`、以及 slip 时的 `slip <virtual帧>`。

### 3.6 效果 `fx`

链地址来自**行首目标**而非参数：`fx …` 作用于焦点 deck，`master fx …` 作用于总线，
`deck1 fx …` 作用于 deck1。

```
[deck|master] fx add <kind>                追加到 bank 末尾（eq|filter|moog|gain|limiter|echo|reverb|flanger|phaser|cut|none，含别名）
[deck|master] fx remove|rm <slot>          只允许删 bank 的最后一个槽（删中间会重编号）
[deck|master] fx select <slot> <kind>        换掉槽里的效果（序号、role、开关状态都不变）
[deck] fx flows <slot> all|none|a,b        flowfx 槽只作用于哪些 flow（stem 名，逗号分隔）
[deck|master] fx toggle <slot>             翻转槽位（引擎持有状态，前端不留镜像）
[deck|master] fx list|ls                   列出槽位：索引、kind、开关、参数
[deck|master] fx set <slot> <param> <value>   设参数（指数平滑，不咔哒）
[deck|master] fx on|off <slot>             接入 / bypass（bypass→engage 会 reset 滤波器）
[deck|master] fx trigger <slot>            触发一次性钩子
[deck|master] fx pad <slot> press|release  按住即 engaged，松开恢复最近意图
[deck|master] fx help|h                    效果清单（从注册表生成，永不与引擎脱节）
```

- `<slot>` 可以是**索引**、**role** 或**效果名**（启动时已 `ListFx` 预载，无需先 `list`）。
  **role 优先**：槽位有一个与内容无关的固定身份（`eq`、`filter`、`echo`…），`fx select` 换掉效果后
  role 不变，所以 `fx set filter value -0.3` 在槽里换成 `moog` 之后仍然指同一个槽
  （`fx list` 会显示成 `filter (moog)`）。效果名按链内第一个同名槽解析。
- MIDI map 的 `fx param` 可以写参数名，也可以写 1-based 序号（`fx param 1`）；序号按效果
  当前声明的参数顺序解析，给可切换效果的 `fx1/fx2/fx3` 指针槽使用。
- 内置效果与参数（`fx help` 输出，来自 `FxKind::ALL` 注册表）：

| kind | 别名 | 参数 |
|---|---|---|
| `eq` | equalizer, tone | `low` `mid` `high` `low_hz` `mid_hz` `high_hz`（增益 −26..+26 dB；分频来自 `[mixer.eq]`，缺省 250Hz / 几何中心 / 2kHz） |
| `filter` | lp, hp, sweep, linear | `value`（−1 = LP 20Hz .. 0 = 全开 .. +1 = HP 18kHz）、`resonance`（0..1 → Q 0.3..18）；2 极点 |
| `moog` | ladder | 同上（同一个 `value`/`resonance` 语义）；4 极点级联，24 dB/oct |
| `gain` | volume, amp | `gain` |
| `limiter` | lim, clip | `ceiling` `release` `recovery` `backoff` `reduction`（`reduction` 是只读表针） |
| `echo` | delay, dub | `mix` `feedback` `time`（拍数，1/32..4）`tone` `pingpong`；反馈路径带阻尼，`pingpong=1` 时左右交叉 |
| `reverb` | verb | `mix` `size` `damping` `width`；Freeverb 拓扑（8 comb + 4 allpass / 声道），comb 长度按引擎率缩放 |
| `flanger` | flange | `mix` `depth` `rate`（每拍周期数，1/16..4）`feedback`（−0.95..0.95） |
| `phaser` | phase | `mix` `depth` `rate` `feedback`（6 级全通级联，系数查表） |
| `cut` | gate, trans | `depth` `div`（拍，1/16..4）`width`（占空比 0.05..0.95）`edge`（ms，0..20）；按拍锁相的梯形门 |
| `none` | empty | 无参数：槽位留着当占位，不处理任何样本（`fx select <槽> none` 用来原地清空、保住序号） |

- 时间类效果（`echo` / `reverb` / `flanger` / `phaser` / `cut`）的节拍参数走
  `FxContext` 的拍网格；曲目还没分析时回落到 120 BPM，不会静音。
- TUI 的 FX 面板把节拍参数显示为 `beats:N`：`N > 0` 是 N 拍，`N < 0` 是 `1/|N|` 拍
  （`-2` = 1/2，`-4` = 1/4），`0` 表示不是节拍参数；`-` / `+` 分别减半 / 加倍。
- 时间类效果的主参数排第一，所以 MIDI 的 `fx param 1` 就是"效果量"（`echo` 的 `mix`、
  `cut` 的 `depth`）。
- 时间类效果在 `prepare` 里按引擎率一次性分配缓冲区，之后 `process` 不再分配；
  `Mixer` 构造、`mixer reload` 和运行中的 `fx add` 都会调用它。

### 3.7 Stems（分离与逐 stem 控制）

`stem separate` 把当前曲目离线分离成 4 条 stem（drums / bass / other / vocals）并热装进 deck。
**分离在后台线程上跑，期间可以继续 play / loop / jump / sync / beatmatch** —— 装上的那一刻是
一次"换源 jump"，所以无缝隙、不丢循环。首次约 80 s（3:39 曲目）/ ~1.9 GB 内存，结果按内容哈希
缓存到**工作目录** `.cache/stems/<key>/`，之后重跑是 0 s；拉取的模型放在 `.model/<name>.onnx`
（`HYPERMIXX_CACHE` / `HYPERMIXX_MODELS` 可覆盖，`stem cache prune` 只清前者）。

| 命令 | 说明 |
|---|---|
| `[deck] stem separate [--model <name\|path>]` | 分离并安装；provider/overlap/shifts 等来自 `config.toml` |
| `[deck] stem status` | 逐 stem 报告：电平、是否被静音/独奏、实际增益 |
| `[deck] stem full \| acapella \| instrumental \| drums \| bass` | 命名编排（改的是静音位，**你的电平保留**） |
| `[deck] stem clear` | 四条全开、unity、取消 solo |
| `[deck] stem cancel` | 停掉正在跑的分离（在**当前模型窗口**结束时停，不是立刻） |
| `[deck] stem cache` | 缓存目录与占用 |
| `[deck] stem cache prune [--keep <n>] \| clear` | 只留最近 n 条（缺省 4）；`clear` = 全清 |
| `[deck] <stem> level <-1..1>` | 该 stem 电平（0 = unity，-1 = 精确静音） |
| `[deck] <stem> mute [on\|off]` | 静音该 stem（solo 仍然优先） |
| `[deck] <stem> solo [on\|off] \| unsolo` | 加入/移出独奏集合（**集合语义**，可多个同时 solo） |
| `[deck] <stem> fx <子命令>` | 效果链是**流组共享**的：写操作广播到每条 flow（一个控件、N 份实例），如 `deck0 vocals fx set filter value 0.3` |

`<stem>` = `drums` \| `bass` \| `other` \| `vocals`。行首目标因此可以带一层 stem：
`deck0 vocals level -0.5`（电平/静音/独奏是逐 stem 的事实）；`deck0 vocals fx …` 只是选
*哪条 flow 的视图*，写操作落到流组（whitelist 见下一片）。

分离参数来自 `profile/config.toml` 的 `[stems]`（三个都会改变产出的音频，所以**都进缓存 key**）：

| 键 | 缺省 | 效果（`stem-test.mp3`, 3:39, 实测） |
|---|---|---|
| `shifts` | 1 | `0` 与 `1` 等价（单趟）；`2` 多跑一次带偏移的平均，**+98% 时间**（81→161 s）、+360 MB |
| `overlap` | 0.25 | `0.0` **−24% 时间**（81→62 s，窗口数 38→29）；代价是窗口边缘的模型估计权重更大，需要耳朵判断 |
| `provider` | `cpu` | `cuda`：**13 s**（vs CPU 87 s，RTX 4050），显存 ~3.2 GB。需要 `--features cuda` 构建 + 宿主 CUDA 13 / cuDNN 9 |

### 3.8 Auto-gain

`config.toml` 的 `[mixer.autogain] enabled/target_db` 决定是否把每首曲子的最响 3 s 归一化到目标电平。
响度在**装载时**从 PCM 测一次（存在 deck 上），增益在引擎里按当前设置推导（±12 dB 钳制），
所以 `mixer reload` 改开关/目标对**已经装载**的曲子立即生效，不需要重测（`Command::SetAutogain`
仍可作会话内覆盖，下一次 `mixer reload` 回到配置值）。它在逐 flow 的 `flow fader` 之后、
channel trim 之前。

`provider = "cuda"` 是**要求而不是建议**：CUDA 不能接管整张图时会直接报错，而不是悄悄在 CPU 上跑
（那样只是慢 9 倍，看起来却像成功了）。窗口长度不可调 —— 导出把它钉在 343,980 采样（7.8 s）。

分离是**离线异步**的：换源在后台线程预热完才落地，期间 deck 一直在放原始混音；重复跑同一条曲目
命中内容寻址缓存，成本是 0 s。缓存每曲约 300 MB，所以 `stem cache prune` 是需要的，不是装饰。
CPU 与 CUDA 的结果 Σ 残差一致（实测都是 −32.9 dB 量级），但两者存在不同的缓存条目里（key 含 provider）。

**每 stem 的链也可以在配置文件里给**（`--config`）：

```toml
[mixer.fx]
filter = "linear"              # linear | moog：固定 filter 级的 kind（`fx select filter moog` 可运行时换）
knob_fx = ["echo"]             # 合并后 knob bank
pad_fx = ["reverb"]            # 合并后 pad bank（在 knob 之后）

[[mixer.fx.flow_fx]]           # 合并前、逐 flow 的插入
kind = "filter"
flows = ["vocals"]             # 只作用于人声；省略 = 全部 flow；[] = 哪条都不作用

[mixer.fader]
position = "pre"               # pre | post：cue send 抽在 fader 的哪一侧
```

`flow_fx` 是**模板而不是一条链**：每条 flow 都有同一组槽位、各自独立的实例（状态互不串味），
但**控制是一份**：参数、开关、pad 状态广播到所有实例；`flows` 决定哪条 flow 真正跑它。
白名单收窄时那条 flow 上的尾巴会被切（不是淡出），放宽时新进来的实例会先 `reset()`，
不会带着旧状态复活。三个 bank 里的名字在构造时校验，写错会在启动时失败而不是等到用的时候。
一个 deck 的平坦槽位序号是 `[eq, filter, flowfx…, knobfx…, padfx…]`。

三条容易混淆的语义，都是刻意的：

- **mute 与 level 是两件独立的事**，而且分在两个串联的阶段上：**mute/solo 是 source fader**
  （每条流最前面的一颗门，正好是 `1.0` 或 `0.0`），**level 是 flow fader**。所以取消静音恢复的
  是你设过的电平，不是 unity；而被静音的 stem 交给它自己的效果链的是**真正的静音**，不是带着
  尾巴的衰减信号。
- **solo 优先于 mute**，而且 solo 是集合：`deck0 vocals solo on` 再 `deck0 drums solo on`
  会同时听到两条，而不是后者顶掉前者。
- **`deck0 fader flow <x>`（不带 stem）一次写全部四条 stem 电平**；`fader deck <x>` 才是那颗
  通道 fader（硬件推子映射到它，位置由 `[channel] fader` 决定）；
  `deck0 vocals fader <x>` 只写一条。

### 3.7.1 边分离边播（分段就是模型自己的分段）

`stem separate` 不在结尾才装 stems，而是**先把四路"可增长"的流装进 deck，再按模型自己的节奏逐块装入**：

- 装上去的那一刻四路之和恰好等于原曲（stream 0 放混音、其余静音），所以你听到的还是那首歌；
- **块的粒度就是模型的分段**：charon 按 7.8 s 窗口扫过轨道、每次前进 `(1−overlap)×7.8 s`，一块一旦不再被
  后续窗口影响就吐出来（默认 overlap 0.25 ⇒ 每块 **5.85 s** 音频）。所以**没有 chunk 长度可设**，
  也没有"第二套切分要和模型对齐"这回事——实测（真实模型）这样装出来的音频与整轨一次性分离**逐样本相同**；
- 第一块在**约 1–2 个窗口的算力**之后就进了 deck：CPU 上 40 s 素材首个块 8 s 后落地，21 s 全部分离完；
- 每块到达即替换它自己那段，**不重启 flow、不停音乐、无接缝**（通过已装好的 `Source` 句柄完成）；
- 边界处 ~50 ms 交叉淡化；播放头越过已装入水位时回落到混音 —— 搓/跳到未装入的尾部照样有声；
- `stem status` 显示 `25% separated` 这样的水位（`DeckState.stems.separated`）。

```
[deck] stem separate [--model <name|path>]
```

- **`stem separate` 不带调参旗标**：模型、provider、`overlap`、`shifts`、`threads`、`low_memory`
  全都是 `config.toml` 的 `[stems]` 键 —— 它们是**这台机器**的属性，不是曲目的属性；编辑后
  `live reload` 立即生效（下一次分离就用新的）。唯一的例外是 `--model <name|path>`：临时换一个
  模型（表里名字，或本地 `.onnx` 路径）而不改配置。
- `overlap` 同时决定块长（`(1−overlap) × 7.8 s`）与窗口重叠：更小 = 更快、块更长、窗口边缘权重更大。
- **`shifts = 2` 不能流式**（运行时拒绝给一个流做时间平移）：那条路退回整轨一块、结尾才装，
  `live show` 会直接这么写。默认 `1` 是流式的。
- 缓存**按块追加**：`meta` 里记已完成水位，所以重跑命中缓存（一句 "was already separated"、0 s），
  中途取消后重跑会从水位处接着做（续跑用**整轨**的输入归一化，所以接缝处的音频与一次跑完相同）。
- 分离期间照常 play/loop/jump/sync；进度既按 % 也按块报告。
- **低端设备**：`[stems] threads` 是**播放**参数 —— 分离占满 CPU 会让音频回调饿死、音乐卡顿；
  `overlap = 0.0` 少跑 ~24% 窗口；`model = "htdemucs-fp16"` 下载少一半（同速）；`low_memory = true`
  （默认）把峰值内存从 5.6 GB 压到 2.1 GB。实测（16 核，40 s 素材）：threads 自动 20 s，
  `threads = 2` 28 s（+40%）但只占 2 核。

### 3.7.2 电平与信号路径

```
每条流:  source fader(门) → flow_fx → flow_fader ─┐
                                                   Σ → [cue tap] → fader → chain → [cue tap] → fader → gain
                        共享 crossfader 的增益坐在三个位置之一（[crossfader] place）
```

一个通道**只有一颗 fader**，位置由 `[channel] fader = "pre_fx" | "post_fx"` 决定：`pre_fx` 是
历史上 flow fader 的位置（先衰减再进音色），`post_fx`（缺省）是 deck fader 的位置（音色随电平
一起衰减）。`flow_fader` 是**每 stem 自己的电平**，不是这颗通道 fader。

- **所有 fader 只衰减**：行程顶端是 0 dB。要 headroom 用 `gain`（trim，0 dB…+6 dB，`0.0` = 0 dB、
  `1.0` = +6 dB），它是设定而不是演出控制。
- **crossfader 的位置可配**（`[crossfader] place = "source" | "flow" | "deck"`），三种曲线
  `classic`（缺省：中间两轨同时 0 dB，两头线性到单轨）/ `constant_power` / `cut`
  （两端陡降，`cut_width` 缺省 0.05 = 各占 5% 行程）。
- **precue 恒在 fader 之前**（不是 post）：推子拉到底只静音房间、不静音耳机，这样才可以把
  下一首 cue 好再推上去。分叉点由 fader 位置唯一确定，没有单独的 `cue_tap` 配置；crossfader
  在分叉点之前时 cue 不受它影响，在之后则跟着它（文档写明）。
- **cue 总线与 master 混合**（`[cue] blend`：`-1` = 只听 cue（缺省）/ `0` = 两者 / `+1` =
  只听 master，`curve`/`cut_width` 同上），这就是耳机上那颗 cue-mix 旋钮。

### 3.8 热加载拓扑 `mixer reload`

```
mixer reload [<file>]   重读拓扑并就地重建；不带路径则读 --config 的文件，否则 ./profile/config.toml
```

- **语义 = 内置默认 ← 文件**：文件里没写的键取**默认值**，而不是保留运行中的值。所以演出中途 reload
  要把想保留的 channel fader / gain 一起写上。
- **保留的只有"不是配置值"的东西**：每个 channel 的 deck 与走带（reload 不会停音乐）、计量计数，
  以及 master/cue trim（它们是运行时推子，不是配置值）。文件里没再出现的 channel 会被丢弃
  （连同它装载的曲目），并打印一行说明。
- **要么整份生效，要么整份拒绝**：未知效果名 / 没有 `[[multideck.deck]]` 都在动任何状态之前失败 ——
  半套拓扑会让引擎停在一个没有任何配置文件能描述的状态里。旧链在验证通过之前一直照常工作。
- 设备绑定也在 reload 里重建并重新列出（见 3.9），所以"插上耳机 → `mixer reload`"一步到位。

### 3.9 输出（设备绑定）

```
output list      （别名 ls；bare `output` 同义）列出每个目的地：名字、角色、状态、绑定的设备、声道
output refresh   （别名 reload/reopen）重试所有不可用的目的地，并重新列出
```

每行形如 `output main  master live  Built-in Audio Analog Stereo`，状态四选一：

| 状态 | 含义 |
|---|---|
| `live` | 流已打开，正在喂设备 |
| `sink` | 宿主根本没有输出设备（CI/容器）；引擎照常跑，写进黑洞 |
| `missing` | `device` 指定的设备现在不在（没插/没通电）。插上后 `output refresh` 即可 |
| `disabled` | 设备存在但**不接受引擎的采样率**（或该输出的 `sample_rates` 不允许引擎率）。不会偷偷换个率跑 |

- `device` 用**名字子串**匹配（大小写不敏感，第一个命中者胜），全数字则按设备序号；省略 = 系统默认
  设备。子串而非硬件 id，因为名字是各平台唯一共通的东西，而 id 换机器就失效。
- `sample_rates` 是该输出**允许**的采样率列表，缺省 = 引擎率本身；引擎率不在里面，或设备不提供它，
  就 `disabled` + 警告。这条是刻意的：48k 设备被"顺手"当成 44.1k 用会让每首歌慢 8.8%，
  比静音更糟——因为它听起来像是故意的。
- 同一物理设备只保留**一条**流（服务器会把两条直接相加，而 cue 总线没有 limiter）：
  第二条降级为 sink 并提示，解决办法是给它一个自己的 `device`。

### 3.10 MIDI

映射文件里的 `action` 就是**一条去掉目标的会话命令**（`"beatjump 4"`、`"vocals mute"`、
`"sync phase"`），目标取 binding 的 `deck` 字段。语法表只有一份
（`hypermixx-core::action::ACTIONS`），`docs/MIDI.md` 的 action 表由它维护；
CLI 侧有一个测试逐一断言每个 action 都有对应的会话动词。

```
controller ports    列出可用控制器输入端口（`midi ports` / `list` 仍是别名）
controller mode     查看/切换已接入映射的 unit 模式
```

只有查询与模式切换走会话命令：打开端口在启动时（`[[controller]].device`/映射文件里的 device）
或 TUI 的 `F9` 设置界面完成——
映射是对运行中引擎接线一次，会话中途不重新打开输入。映射表格式与全部 action 见
[`MIDI.md`](MIDI.md)。

### 3.11 本机设置 `live`

```
live show             （bare `live` 同义）打印当前生效的设置：引擎采样率、ring/prefill、stem 默认值
live reload [<file>]  重读 config 的 [live] 段（缺省当前 config 路径）
```

`config.toml` 的 `[live]` 段（启动时读）：

| 键 | 缺省 | 说明 |
|---|---|---|
| `[audio] sample_rate` | 44100 | 引擎渲染率，同时也是向每个设备请求的率；素材在装载时重采样到它。填 48000 等任何设备支持的率都可以（8 kHz–384 kHz） |
| `[audio] ring_frames` | 4096 | 输出 ring 容量（帧）：更多余量 = 更多延迟；随附的 `profile/config.toml` 用 1024（~23 ms） |
| `[audio] prefill_frames` | 2048 | 开流前预填的静音，避免设备等第一个块；上限为 ring 的一半；随附 profile 用 512 |
| `[audio] device_buffer_frames` | 后端默认 | 以 `BufferSize::Fixed(n)` 交给 cpal；ALSA 的 period 为 `n/4`。随附 profile 用 512（~2.9 ms period / ~11.6 ms buffer）；`0` 表示后端默认 |
| `[stems] model` | htdemucs | 模型名（`stem model list` 可列） |
| `[stems] provider` | cpu | `cpu` 或 `cuda`（`--features cuda` 构建） |
| `[stems] overlap` / `shifts` | 0.25 / 1 | 分离窗口重叠与集成趟数。`overlap` 同时决定块长 `(1−overlap)×7.8s`；`shifts 1` 才流式，`2` 退回整轨一块 |
| `[stems] threads` / `low_memory` | 0 / true | 分离可用的线程数（0 = 自动）与内存取舍（低内存档峰值 5.6→2.1 GB，代价 ~11% 速度） |
| `[stems] model` | htdemucs | 模型名，或本地 `.onnx` 路径（`stem model list` 列名字） |

- **采样率、ring 与设备 buffer 需要重启**：引擎的时钟与缓冲在启动时定下（`live reload` 会明说这一点），
  但 `[stems]` 的默认值下一次分离就生效。未知键报错（拼错的 `sample_rat` 静默按 44.1k 跑才是灾难），
  越界的值被夹到有效范围并打印一行警告。
- **分离跟着引擎率走**：模型固定 44.1 kHz，引擎不是 44.1k 时 `stem separate` 会把 mix 转到模型率、
  把每块 stem 转回引擎率（日志里明说），缓存仍按模型率存，所以同一首歌在两个率下共用一条缓存。
  多花两趟重采样，除此之外行为不变。
- `[audio] block_frames` **不在这里**，因为它是引擎内部契约（时间拉伸引擎按块喂入），不是本机性能参数。
- **分离的块长也不在这里**：它就是模型自己的窗口步长（见 §3.7.1），设不了的才是对的。

### 3.12 配置文件

配置是**一个文件**，四个段落：

```toml
[multideck]    # controller 映射 + per-deck 参数；第 N 行即 deckN，数量由 --4-decks 决定
[[multideck.deck]]
source = "ctl1.A"         # <controller>.<face>[.mirror]；省略 = 该 deck 无控制器
side = "left"             # crossfader 左/右；center = 不参与
cue_send = 1.0            # 送 cue 总线 0..1
# flow_fader = 0.0        # 逐 flow 电平
# deck_fader = 0.0        # channel fader 起点
# gain = 0.0              # trim

[mixer]        # 总线：[mixer.fx] / [mixer.eq] / [mixer.limiter] / [mixer.fader] / [[mixer.output]]
[mixer.crossfader]
curve = "classic"         # classic | constant_power | cut
# cut_width / start 仍可写；省略 = 内置默认

[mixer.fx]                # 四个 FX bank + 固定 sweep；所有 deck 共享
master_fx = []            # master 效果，limiter 之前
flow_fx = []              # 逐 flow insert（模板：每 flow 一条独立链）
knob_fx = []              # 合并后 knob bank
pad_fx = []               # 合并后 pad bank
filter = "linear"         # linear | moog — 固定 sweep 的种类
# 逐 flow 白名单用 [[mixer.fx.flow_fx]]：
# [[mixer.fx.flow_fx]]
# kind = "filter"
# flows = ["vocals"]      # 省略 = 全部 flow，[] = 哪条都不作用

[mixer.eq]
lower_hz = 250            # low/mid 分界
higher_hz = 2000          # mid/high 分界；中频 bell 在两者的几何中心

[mixer.limiter]
limiter = true            # 安全限幅，默认开启；写 false 才关
ceiling_db = -1.0         # 限幅门限 dBFS（−40 .. +12），省略 = −1

[mixer.fader]
position = "pre"          # pre | post — cue 送在 fader 哪一侧
crossfader_curve = "classic"  # cue/master 混合曲线；等价于 [mixer.cue].curve，新分组优先
cue_blend = -1.0          # -1 只听 cue，0 各半，+1 只听 master

[mixer.autogain]
enabled = false
target_db = -12.0
[live]         # 本机设置：[audio] / [keylock] / [stems]
[preference]   # 前端设置：波形、主题、曲库

[[controller]] # 每个控制器一个会话
id = "ctl1"
map = "mapping.toml"      # unit-map 文件指针，相对本文件所在目录
# device = "..."          # 可选，覆盖映射文件的 [meta].device
```

- deck 编号分两层：audio 侧是 `deck1..deckN`，数量由 `--4-decks` 决定（2 或 4，缺省 2）；
  配置里的第 N 行 `[[multideck.deck]]` 只描述 deckN——`source` 把 controller 逻辑面映射到物理
  deck，行数不足时多出来的 deck 没有 controller。controller 侧是逻辑面 `A`/`B`/`C`/`D`
  （mapping 里 `deck = "A"`）。
- mirror：`source = "ctl1.A"` 与 `source = "ctl1.A.mirror"` 分别占两个 deck；`mirror-deck`
  preset 是 **deck unit**，每个 deck strip 装一个 toggle，按下只切这一组控件到它的 mirror 面，
  两条 deck strip 可以各自独立镜像。
- crossfader 的左右组合由 controller 侧定义：mapping 的 `global.crossfader` 用多个 mode
  （`fader cross A B` / `fader cross Am Bm`）指定 deck 对；`[mixer.crossfader]` 只剩
  `curve` / `cut_width` / `start`（`place` 已固定为通道末端）。
- 旧的 `[[mixer.channel]]` / root 级 `[[channel]]` / `[mixer.chain]` / `[mixer.master]` 都不再兼容：
  deck 的字段写进 `[[multideck.deck]]`，效果写进 `[mixer.fx]`。
- FX 槽位的身份是 **role + index**，不是当前效果名：配置里的列表只决定启动时的初始效果和槽位
  数量；运行时用 `fx select <slot> <kind>` 换效果，role、序号、开关、pad latch、flow 白名单都保留，
  同名参数（如 `filter`/`moog` 的 `value`/`resonance`）按名继承。映射应绑 `fx param 1` 或 role，
  不要绑效果名。想预留一个空槽就写 `"none"`，不要用 `fx remove` 删中间槽。

- 不给 `--config`：Linux/Termux 用 `$XDG_CONFIG_HOME/hypermixx/config.toml`
  （缺省 `~/.config/hypermixx/config.toml`），Windows 用 `%APPDATA%/Hypermixx/config.toml`；
  `HYPERMIXX_CONFIG` 可覆盖。
- 路径不存在时首次启动从内置模板复制一份；已存在绝不覆盖；写不了就用内置模板继续跑。
  仓库里的 `profile/config.toml` 就是模板，`profile/mapping.toml` 是 unit-map 样例。
- 会话内 `config show` 看当前配置，`config reload [file]` 重读；`mixer reload` / `live reload`
  作用于同一份文件。
- 段名拼错（`[mixr]`）是错误，不是静默回落 —— 否则看起来用了某套配置其实没有。
- `[preference]` 是空段也合法（= 什么都没设）；波形、主题、曲库见
  `[preference.waveform]` / `[preference.theme]` / `[preference.library]`。
- `[preference.analysis] auto_analyse = true`：browser 载入时，如果既没有外部曲库网格
  （Rekordbox/Serato/VDJ）也没有分析缓存，就在后台自动跑一次分析并写回缓存；播放不会等它。
  缺省 `false`。

### 3.13 帮助与退出

```
help | h | ?      命令总表（含启动参数速览）
quit | exit       退出（REPL 里 Ctrl-D / EOF 等效；TUI 里 Ctrl+C）
```

---

## 4. TUI 专属

### 4.1 UI 命令（不进引擎）

```
zoom in | out | fit     波形缩放（fit 为缺省；仅 TUI，REPL 会报 unknown command）
load                    （不带路径）弹文件浏览器，见 §3.1
```

### 4.2 键位

| 键 | 行为 |
|---|---|
| `Enter` | 执行；补全弹窗高亮项与已输入词不同时改为“应用补全” |
| `Tab` / `Shift+Tab` | 切换焦点 deck（补全弹窗开着时 = 接受/回退补全） |
| `↑` `↓` | 历史（弹窗开着时移动选择） |
| `←` `→` `Home` `End` | 移动光标 |
| `Backspace` `Delete` | 编辑 |
| `Esc` | 关补全弹窗；再按清空命令行 |
| `PgUp` `PgDn` | 日志区滚动 5 行 |
| `F9` | 打开设置界面（控制器端口/映射文件、拓扑文件、输出绑定、live/preference 文件） |
| `Ctrl+C` | 退出 |

浏览器（启动时焦点所在）用 `↑` `↓` 选择、`→`/`Enter` 打开、`←`/`Esc` 返回；track 上
`Enter` 加载，双击弹出 deck 选择框。控件也可鼠标点击（pads、fader、knob、crossfader）。

- 命令行有上下文补全：动词、目标词、`sync`/`loop`/`fx` 子命令、槽位名（带 `slot N` 提示）、
  参数名、`load` 的文件路径。
- 端口/文件选择器打开时独占键盘。

### 4.3 输出区

- 引擎响应、后台通知（decode/analyse）、命令回显 `> …` 都推进滚动日志。
- deck 头行带每 deck 一个色相的边框，焦点加粗。

---

## 5. `state` 输出格式

一行一个 deck（`response::deck_line`），TUI 与 CLI 完全一致：

```
deck0  playing 1:23.456 / 7:00.648  [323456/18462369]  122.0 BPM → 128.1 BPM  8A  keylock on  loop [100000-200000]  sync phaselock  122.0 BPM ← deck0  phase pid  locked
└deck┘ └transport┘ └当前/总时长┘ └当前帧/总帧┘ └网格 BPM → 当前 BPM┘ └key┘ └keylock┘ └可选：key 位移/循环/武装/slip/sync 徽标┘
```

- **当前 BPM** = `DeckState.bpm_at_frame × DeckState.tempo`（**不含** nudgerate/playing_rate——
  bend 与相位修正是瞬态，不是 tempo）。`grid` 取 audible 位置的局部网格 BPM；两者相差
  ≥0.05 时才追加 `→ x.x BPM`，所以恒速（tempo=1）下状态行与原格式一字不差。
  `response::bpm_label` 同时给 `deck_line` 和 TUI deck 头用，两端永不脱节。
- transport：`empty` / `playing` / `paused`。
- sync 徽标（`sync_badge`）仅在有组速度、bend 或 leader 时出现，包含
  `sync <mode>`、组 BPM、`← deckN`（leader 不显示箭头）、`phase <mode>`、
  `bend ±x.xxx`、`pll ±x.xxx`（纯相位校正）、`locked`。
- 徽标永远排在 `[当前/总帧]` 之后，保脚本解析（`phase_probe.sh` / `sync_phase.sh`）。

---

## 6. 示例会话

```text
hypermixx> load test.mp3 122
deck0 loaded: 18462369 frames (7:00.648)
hypermixx> play
hypermixx> beatjump 16
hypermixx> fx set eq low -0.5
hypermixx> fx set filter value 0.5
hypermixx> master fx list

# deck1 同步到 deck0：先对 BPM 再用 PI 追相位
hypermixx> deck1 sync phaselock pid
hypermixx> deck1 sync tempolock
hypermixx> deck0 sync set-leader
hypermixx> deck1 bend 0.04 0.5      # 临时 +4%，0.5s 后自动松开
hypermixx> deck1 sync unlock         # 解锁，tempo 保留
hypermixx> deck0 loop quantum eighth
hypermixx> deck0 loop 8
hypermixx> deck0 loop halve
hypermixx> deck0 loop exit
hypermixx> quit
```

---

## 7. 相关文档

- [`../README.md`](../README.md) — 安装、架构速览、脚本实测数据
- [`../ARCHITECTURE.md`](../ARCHITECTURE.md) — 六 crate 分层、引擎/混音/同步内部设计
- [`MIDI.md`](MIDI.md) — MIDI 映射 TOML schema、action 注册表、guide 向导
