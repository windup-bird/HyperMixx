# Hypermixx 命令参考

REPL 与 `--tui` 共用同一套语法（`crates/hypermixx-cli/src/command.rs`）与同一套响应渲染
（`response.rs`），本文档覆盖：启动参数、会话语法、全部会话命令、TUI 键位与输出格式。
MIDI 映射文件的 action 表见 [`midi-mapping.md`](midi-mapping.md)。

---

## 1. 启动参数

```bash
cargo run -p hypermixx-cli -- [flags]
```

| 参数 | 说明 |
|---|---|
| `--tui` | 终端界面（默认是行式 REPL） |
| `--config <file>` | 自定义混音拓扑 TOML（通道/效果链/输出）；缺省用内置 `simple_dj()` 参考拓扑 |
| `--print-config` | 打印参考拓扑 TOML 后退出（不启动引擎） |
| `--backend auto\|stratum\|timestretch` | 离线分析后端，缺省 `auto`（stratum 优先，失败回落） |
| `--midi <port>` | 启动时打开 MIDI 输入端口（端口号或名字，`midi ports` 可列） |
| `--midi-map <file>` | 映射文件，缺省 `midi-map.toml`；**必须与 `--midi` 同给**，否则报错 |
| `--midi-guide [<file>]` | learn 模式地图编辑器（**不启动音频引擎**）；文件缺省 `midi-map.toml` |
| `--decks <n>` | 仅作用于 `--midi-guide` 的目标清单宽度，缺省 2 |

- 未知参数是硬错误（`error: unknown argument …`），不会静默忽略。
- `--midi-guide` 即使不跟 `--midi` 也能在 TUI 里选端口/文件；`--tui` 下 `F2` 选端口、
  `F3` 选映射文件，`--midi/--midi-map` 只是免选择的快捷方式。
- deck 数量启动时从引擎查询（自定义拓扑可配任意通道数），不是固定常量。

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

- 帧 = 1/44100 秒（引擎域固定 44.1 kHz 立体声）；`jump` 用帧，`beatjump`/`loop`/`sync` 用拍。

---

## 3. 会话命令

### 3.1 载入与分析

```
[deck] load <path> [bpm]    后台解码装载；bpm > 0 则建常网格、跳过分析
[deck] analyse              后台跑分析器并发布网格（analyze 同义）
```

- `load` 的 `bpm` 非数字 → 前端直接报 `bpm must be a number`。
- TUI 里 `load` **不带路径**会弹文件浏览器（支持 mp3/wav/flac/ogg/m4a 等）。
- `analyse` 用 `--backend` 选定的后端；完成提示形如
  `[analyse] deck0: 122.0 BPM, 8A`。
- 装载是后台线程，立即回 `[load] deck0: decoding …`；完成回
  `deck0 loaded: 18462369 frames (7:00.648)`。

### 3.2 走带

```
[deck] play                  切换播放/暂停（toggle；`pause` 已移除）
[deck] cue [play|back|set|smart]
                             cue 点：`play` 从 cue 播放，`back` 回 cue 并暂停，`set` 把当前帧
                             记为 cue 点，`smart` = 引擎按走带决定（播放中→back，暂停中→set）
                             **bare `cue` = 整颗 cue 键**：第一次是按下、再一次是松开
                             （状态在引擎里，所以前端不需要自己记走带）
[deck] vinyl [touch|release|turn <ticks>]
                             唱盘：`touch` = 手放上去（暂停并记住走带），`turn` = 转轮（手持时
                             按 tick 移动播放头，空手时是 pitch bend），`release` = 松手（恢复
                             被 touch 打断的走带）。bare = touch
[deck] jump <frame>         按帧 seek（1 秒 = 44100 帧）
[deck] beatjump <beats>     按整拍 seek，保持相位（i64，负数回跳）
[deck] tempo <ratio>        设 tempo 绝对值（1.0 = 原速，0.25..4.0）
[deck] tempofader <-1..1>   推子位置，tempo = 1 + pos × temporange（MIDI 走这条）
[deck] temporange <0..1>    推子满量程（缺省 0.1 = ±10%）；只改映射，不动当前 tempo
[deck] keylock on|off|wide  变调引擎（缺省 on；off = tape 零延迟直通，wide = widekeylock）
[deck] key <semitones>      变调（**占位**：只记录并回读，暂不发声）
state                       打印全部 deck 的状态行
```

- `cue` 点默认在装载原点（帧 0）；`Load` 换 deck 时回到 0。`back`/`play`/`hold` 走非阻塞
  jump，落地可能滞后一个块 + 预热时间（`hold` 在**暂停**那一路不跳，所以是即时的）。
- **`vinyl`（唱盘）**：一颗转盘在真实设备上是两个控件 —— 一个 note（贴盘传感器）+ 一个相对
  CC（轮子本身）。两个绑定都写 `action = "vinyl"`，**转一下到底什么意思由引擎判定**（只有它
  知道此刻手在不在盘上）：
  - **手在盘上** → 暂停 + 每块把播放头**钉到**手搓到的位置（每 tick `step` 帧，缺省 441 =
    10 ms）。**只有轮子动的那一块出声**：移动可闻、静止静音 —— 真唱盘静止时也没有信号（而不是
    把同一个颗粒反复播成嗡嗡声）。**暂停中的 deck 一样能搓**（照样出声），松手后仍是暂停。
    反向能搓（引擎只前进，位置由 deck 自己记账，所以后退不会被"格内前进"抵消）。
  - **手不在盘上** → pitch bend，**累加**：每条消息按 tick 数加减速度（上限 ±10%），不是延长
    时间。
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
[deck] sync phase <mode> [seconds]       先对 BPM，再追相位（phase ⊃ tempo）
[deck] sync tempolock                    双向共享一个 tempo，任一边 fader 都带动对方（别名 lock）
[deck] sync phaselock <mode> [seconds]   单向：follower 精确跟随 leader（其 fader 被忽略）
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

- 两 deck 互相同步会拒绝第二条；显式 leader 拒绝自同步；**leader 必须有网格**。
- `sync tempo` 在 `tempolock`/`phaselock` 锁下直接拒绝（写了也会被 `group_bpm` 盖回）；
  `sync phase` 在锁下跳过对拍那步、只装 align（锁定期间改相位模式是合法操作）。
- `sync unlock` 解的是**整组**（`[deck_id, leader, follower]` 一起清），但 `tempo` 与
  `leader` 都保留。
- 实测收敛特性见 README 的 `sync_phase.sh` 表格。

### 3.4 手动微调 `nudge`

```
[deck] nudge <delta> <seconds>   临时速率弯折（0.04 = +4%），到点自动松开；**秒数必填**
[deck] nudge off                 立即松开（ramp 回零，不咔哒；别名 stop/release/reset）
```

只动 `nudgerate`，结束时 `tempo` 原封不动；`nudgerate` 总限幅 ±15%（PLL ±5% + nudge ±10%）。
MIDI 的按住弯折用 momentary `nudge` binding；要定时就把 `seconds` 写进 mapping（见
[`midi-mapping.md`](midi-mapping.md)）。

### 3.5 循环 `loop`

```
[deck] loop in                    武装 in 点（按拍量化），后台预热 LoopFlow
[deck] loop out                   按量化后的 out 点切入，零延迟
[deck] loop <beats>               N 拍循环（u64 ≥ 1）；循环中 = 只调 out 到 in+N
[deck] loop exit                  退出循环，无缝续播越过 out（别名 off）
[deck] loop cancel                丢弃已武装的 in
[deck] loop halve | double        当前长度 ÷2 / ×2，域 1/32..64 拍（别名 /2 ÷2 / x2 *2 ×2）
[deck] loop edit len <beats>      设长度（保留 in，可小数到一个 quantum）
[deck] loop edit move <beats>     整段平移（in/out 同移，整拍）
[deck] loop edit in <beats> | out <beats>   只动其中一端（别名 length|size / shift）
[deck] loop quantum <q>           out 点量化粒度：beat | half | quarter | eighth
                                  （别名 b 1 / 1/2 2 / 1/4 4 / 1/8 8；缺省 beat）
```

- 循环中编辑 = 整段 Range 一次原子 store，不换流、无间隙。
- `state` 行会显示 `loop [in-out]`、`in armed@帧`、以及 slip 时的 `slip <virtual帧>`。

### 3.6 效果 `fx`

链地址来自**行首目标**而非参数：`fx …` 作用于焦点 deck，`master fx …` 作用于总线，
`deck1 fx …` 作用于 deck1。

```
[deck|master] fx add <kind>                追加到链尾（eq|filter|gain|limiter，含别名）
[deck|master] fx remove|rm <slot>          删除该槽，后续槽位前移
[deck|master] fx list|ls                   列出槽位：索引、kind、开关、参数
[deck|master] fx set <slot> <param> <value>   设参数（指数平滑，不咔哒）
[deck|master] fx on|off <slot>             接入 / bypass（bypass→engage 会 reset 滤波器）
[deck|master] fx trigger <slot>            触发一次性钩子
[deck|master] fx pad <slot> press|release  按住即 engaged，松开恢复最近意图
[deck|master] fx help|h                    效果清单（从注册表生成，永不与引擎脱节）
```

- `<slot>` 可以是**索引**或**效果名**（启动时已 `ListFx` 预载，无需先 `list`）。
  效果名按链内第一个同名槽解析。
- 内置效果与参数（`fx help` 输出，来自 `FxKind::ALL` 注册表）：

| kind | 别名 | 参数 |
|---|---|---|
| `eq` | equalizer, tone | `low` `mid` `high` `low_hz` `mid_hz` `high_hz`（增益 −26..+26 dB；分频 320Hz / 1.2kHz / 3.6kHz） |
| `filter` | lp, hp, sweep | `value`（−1 = LP 20Hz .. 0 = 全开 .. +1 = HP 18kHz）、`resonance`（0..1 → Q 0.3..18） |
| `gain` | volume, amp | `gain` |
| `limiter` | lim, clip | `ceiling` `release` `recovery` `backoff` `reduction`（`reduction` 是只读表针） |

- `fx set` 的 value 非数字 → 前端直接报错。

### 3.7 Stems（分离与逐 stem 控制）

`stem separate` 把当前曲目离线分离成 4 条 stem（drums / bass / other / vocals）并热装进 deck。
**分离在后台线程上跑，期间可以继续 play / loop / jump / sync / beatmatch** —— 装上的那一刻是
一次"换源 jump"，所以无缝隙、不丢循环。首次约 80 s（3:39 曲目）/ ~1.9 GB 内存，结果按内容哈希
缓存到 `~/.cache/hypermixx/stems/`，之后重跑是 0 s。

| 命令 | 说明 |
|---|---|
| `[deck] stem separate [--shifts <n>] [--overlap <x>] [--gpu]` | 分离并安装；见下表 |
| `[deck] stem status` | 逐 stem 报告：电平、是否被静音/独奏、实际增益 |
| `[deck] stem full \| acapella \| instrumental \| drums \| bass` | 命名编排（改的是静音位，**你的电平保留**） |
| `[deck] stem clear` | 四条全开、unity、取消 solo |
| `[deck] stem cancel` | 停掉正在跑的分离（在**当前模型窗口**结束时停，不是立刻） |
| `[deck] stem cache` | 缓存目录与占用 |
| `[deck] stem cache prune [--keep <n>] \| clear` | 只留最近 n 条（缺省 4）；`clear` = 全清 |
| `[deck] <stem> level <-1..1>` | 该 stem 电平（0 = unity，-1 = 精确静音） |
| `[deck] <stem> mute [on\|off]` | 静音该 stem（solo 仍然优先） |
| `[deck] <stem> solo [on\|off] \| unsolo` | 加入/移出独奏集合（**集合语义**，可多个同时 solo） |
| `[deck] <stem> fx <子命令>` | **只作用于该 stem 的**效果链，如 `deck0 vocals fx add filter` |

`<stem>` = `drums` \| `bass` \| `other` \| `vocals`。行首目标因此可以带一层 stem：
`deck0 vocals level -0.5`、`deck0 vocals fx add filter`。

分离参数（三个都会改变产出的音频，所以**都进缓存 key**）：

| 参数 | 缺省 | 效果（`stem-test.mp3`, 3:39, 实测） |
|---|---|---|
| `--shifts <0..=2>` | 1 | `0` 与 `1` 等价（单趟）；`2` 多跑一次带偏移的平均，**+98% 时间**（81→161 s）、+360 MB |
| `--overlap <0..=0.5>` | 0.25 | `0.0` **−24% 时间**（81→62 s，窗口数 38→29）；代价是窗口边缘的模型估计权重更大，需要耳朵判断 |
| `--gpu` | 关 | CUDA：**13 s**（vs CPU 87 s，RTX 4050），显存 ~3.2 GB。需要 `--features cuda` 构建 + 宿主 CUDA 13 / cuDNN 9 |

`--gpu` 是**要求而不是建议**：CUDA 不能接管整张图时会直接报错，而不是悄悄在 CPU 上跑（那样只是慢 9 倍，
看起来却像成功了）。窗口长度不可调 —— 导出把它钉在 343,980 采样（7.8 s）。

分离是**离线异步**的：换源在后台线程预热完才落地，期间 deck 一直在放原始混音；重复跑同一条曲目
命中内容寻址缓存，成本是 0 s。缓存每曲约 300 MB，所以 `stem cache prune` 是需要的，不是装饰。
CPU 与 CUDA 的结果 Σ 残差一致（实测都是 −32.9 dB 量级），但两者存在不同的缓存条目里（key 含 provider）。

**每 stem 的链也可以在配置文件里给**（`--config`）：

```toml
[[channel]]
deck_fx = ["eq", "filter"]     # 共享：整 deck 一条链
flow_fx = ["gain"]             # 模板：每条 stream 各建一条独立的链（4 stems = 4 条）
[channel.stem_fx]              # 覆写：这里写了的 stem 用它，其余继承模板
vocals = ["gain", "filter"]
```

`flow_fx` 是**模板而不是一条链**：`flow_fx = ["filter"]` 意味着四个独立的滤波器（各自状态，
互不串味），不是一个滤波器加在求和之后。`[channel.stem_fx]` 里的名字和 `flow_fx` 一样在构造时
校验，写错会在启动时失败而不是等到用的时候。

三条容易混淆的语义，都是刻意的：

- **mute 与 level 是两件独立的事**：取消静音恢复的是你设过的电平，不是 unity。
- **solo 优先于 mute**，而且 solo 是集合：`deck0 vocals solo on` 再 `deck0 drums solo on`
  会同时听到两条，而不是后者顶掉前者。
- **level/mute/solo 走的是逐 stream 的 flow fader**，`deck0 fader <x>`（不带 stem）一次写全部
  四条，所以单个硬件推子仍然可用。

### 3.8 MIDI

```
midi ports    列出可用 MIDI 输入端口（别名 list；无端口则提示 midi: no input ports）
```

只有列端口走会话命令：打开端口在启动时（`--midi`）或 TUI 的 `F2` 完成——映射是
对运行中引擎接线一次，不支持会话中途开关。映射表格式与全部 action 见
[`midi-mapping.md`](midi-mapping.md)。

### 3.9 帮助与退出

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
| `F2` | 选 MIDI 端口（Enter 连接，Esc 取消） |
| `F3` | 选 MIDI 映射文件并连接 |
| `Ctrl+C` | 退出 |

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
  nudge 与相位修正是瞬态，不是 tempo）。`grid` 取 audible 位置的局部网格 BPM；两者相差
  ≥0.05 时才追加 `→ x.x BPM`，所以恒速（tempo=1）下状态行与原格式一字不差。
  `response::bpm_label` 同时给 `deck_line` 和 TUI deck 头用，两端永不脱节。
- transport：`empty` / `playing` / `paused`。
- sync 徽标（`sync_badge`）仅在有组速度、nudge 或 leader 时出现，包含
  `sync <mode>`、组 BPM、`← deckN`（leader 不显示箭头）、`phase <mode>`、
  `nudge ±x.xxx`、`pll ±x.xxx`（纯相位校正）、`locked`。
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
hypermixx> deck1 sync phase pid
hypermixx> deck1 sync tempolock
hypermixx> deck0 sync set-leader
hypermixx> deck1 nudge 0.04 0.5     # 临时 +4%，0.5s 后自动松开
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
- [`midi-mapping.md`](midi-mapping.md) — MIDI 映射 TOML schema、action 注册表、guide 向导
