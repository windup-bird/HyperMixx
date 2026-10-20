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
| `--config <file>` | 自定义混音拓扑 TOML（通道/效果链/输出）；不给则读 `./profile/mixer.toml`（缺文件/坏文件回落内置参考拓扑）。这个路径也是 `mixer reload` 不带参数时的默认 |
| `--print-config` | 打印参考拓扑 TOML 后退出（不启动引擎） |
| `--profile <name\|file>` | 一个**指路文件**：写 `mixer` / `live` / `midi`（各自是路径，`midi` 也可内联成表）。裸名字解析成 `profile/<name>.toml`；不给则读 `profile/profile.toml`（没有就用文件夹默认）。显式 flag 优先于它 |
| `--live <file>` | 本机设置 TOML（引擎采样率、ring 大小、stem 默认值）；缺省 `./profile/live.toml`，两边都没有则用内置默认 |
| `--backend auto\|stratum\|timestretch` | 离线分析后端，缺省 `auto`（stratum 优先，失败回落） |
| `--midi <port>` | 启动时打开 MIDI 输入端口（端口号或名字，`midi ports` 可列） |
| `--midi-map <file>` | 映射文件，缺省 `midi-map.toml`；显式给时必须与 `--midi` 同给，否则报错（profile 里的 `[midi]` 不受此限） |
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
[deck] vinyl [touch|release|slip|turn <ticks>]
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
  - **手在盘上** → 暂停 + 每块把播放头**钉到**手搓到的位置（每 tick `frames_per_tick` 帧，缺省 1323
    ＝ 一张 33⅓ 唱片转一圈 1.8 s ÷ 60 格）。**只有轮子动的那一块出声**：移动可闻、静止静音 —— 真唱盘静止时也没有信号（而不是
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
[deck] loop in                    武装 in 点（按拍量化），后台准备 looper
[deck] loop out                   按量化后的 out 点硬切入
[deck] loop <beats>               N 拍循环（u64 ≥ 1）；循环中 = 只调 out 到 in+N
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
[deck|master] fx add <kind>                追加到链尾（eq|filter|gain|limiter，含别名）
[deck|master] fx remove|rm <slot>          删除该槽，后续槽位前移
[deck|master] fx toggle <slot>             翻转槽位（引擎持有状态，前端不留镜像）
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
缓存到**工作目录** `.cache/stems/<key>/`，之后重跑是 0 s；拉取的模型放在 `.model/<name>.onnx`
（`HYPERMIXX_CACHE` / `HYPERMIXX_MODELS` 可覆盖，`stem cache prune` 只清前者）。

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

- **mute 与 level 是两件独立的事**，而且分在两个串联的阶段上：**mute/solo 是 source fader**
  （每条流最前面的一颗门，正好是 `1.0` 或 `0.0`），**level 是 flow fader**。所以取消静音恢复的
  是你设过的电平，不是 unity；而被静音的 stem 交给它自己的效果链的是**真正的静音**，不是带着
  尾巴的衰减信号。
- **solo 优先于 mute**，而且 solo 是集合：`deck0 vocals solo on` 再 `deck0 drums solo on`
  会同时听到两条，而不是后者顶掉前者。
- **`deck0 fader flow <x>`（不带 stem）一次写全部四条**，所以单个硬件推子仍然可用；
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
  全都是 `live.toml` 的 `[stems]` 键 —— 它们是**这台机器**的属性，不是曲目的属性；编辑后
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
                                                  Σ → deck_fx → deck_fader → gain(trim) → 求和
                        共享 crossfader 的增益坐在这三颗 fader 之一的位置（[crossfader] place）
```

- **所有 fader 只衰减**：行程顶端是 0 dB。要 headroom 用 `gain`（trim，0 dB…+6 dB，`0.0` = 0 dB、
  `1.0` = +6 dB），它是设定而不是演出控制。
- **crossfader 的位置可配**（`[crossfader] place = "source" | "flow" | "deck"`），三种曲线
  `classic`（缺省：中间两轨同时 0 dB，两头线性到单轨）/ `constant_power` / `cut`
  （两端陡降，`cut_width` 缺省 0.05 = 各占 5% 行程）。
- **cue 从三颗 fader 之一分叉**（`[channel] cue_tap = post_source_fader | post_flow_fader |
  post_deck_fader`）。分叉点与 crossfader 位置相同时，cue **不受** crossfader 影响（历史上的
  保证）；分叉点在 crossfader 之后时，cue 会跟着它（把 crossfader 放早的代价，文档写明）。
- **cue 总线与 master 混合**（`[cue] blend`：`-1` = 只听 cue（缺省）/ `0` = 两者 / `+1` =
  只听 master，`curve`/`cut_width` 同上），这就是耳机上那颗 cue-mix 旋钮。

### 3.8 热加载拓扑 `mixer reload`

```
mixer reload [<file>]   重读拓扑并就地重建；不带路径则读 --config 的文件，否则 ./profile/mixer.toml
```

- **语义 = 内置默认 ← 文件**：文件里没写的键取**默认值**，而不是保留运行中的值。所以演出中途 reload
  要把想保留的推子/trim 一起写上。
- **保留的只有"不是配置值"的东西**：每个 channel 的 deck 与走带（reload 不会停音乐）与计量计数。
  文件里没再出现的 channel 会被丢弃（连同它装载的曲目），并打印一行说明。
- **要么整份生效，要么整份拒绝**：未知效果名 / 没有 `[[channel]]` 都在动任何状态之前失败 ——
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
`"sync phase pid"`），目标取 binding 的 `deck` 字段。语法表只有一份
（`hypermixx-core::action::ACTIONS`），guide 的目标清单与 `docs/midi-mapping.md` 都由它生成；
CLI 侧有一个测试逐一断言每个 action 都有对应的会话动词。

```
midi ports    列出可用 MIDI 输入端口（别名 list；无端口则提示 midi: no input ports）
```

只有列端口走会话命令：打开端口在启动时（`--midi`）或 TUI 的 `F2` 完成——映射是
对运行中引擎接线一次，不支持会话中途开关。映射表格式与全部 action 见
[`midi-mapping.md`](midi-mapping.md)。

### 3.11 本机设置 `live`

```
live show             （bare `live` 同义）打印当前生效的设置：引擎采样率、ring/prefill、stem 默认值
live reload [<file>]  重读 live 文件（缺省 --live 的路径，否则 ./profile/live.toml）
```

`profile/live.toml`（启动时读，`--live <file>` 可换）：

| 键 | 缺省 | 说明 |
|---|---|---|
| `[audio] sample_rate` | 44100 | 引擎渲染率，同时也是向每个设备请求的率；素材在装载时重采样到它 |
| `[audio] ring_frames` | 4096 | 输出 ring 容量（帧）：更多余量 = 更多延迟（44.1k 下 ~93 ms） |
| `[audio] prefill_frames` | 2048 | 开流前预填的静音，避免设备等第一个块；上限为 ring 的一半 |
| `[stems] model` | htdemucs | 模型名（`stem model list` 可列） |
| `[stems] provider` | cpu | `cpu` 或 `cuda`（`--features cuda` 构建） |
| `[stems] overlap` / `shifts` | 0.25 / 1 | 分离窗口重叠与集成趟数。`overlap` 同时决定块长 `(1−overlap)×7.8s`；`shifts 1` 才流式，`2` 退回整轨一块 |
| `[stems] threads` / `low_memory` | 0 / true | 分离可用的线程数（0 = 自动）与内存取舍（低内存档峰值 5.6→2.1 GB，代价 ~11% 速度） |
| `[stems] model` | htdemucs | 模型名，或本地 `.onnx` 路径（`stem model list` 列名字） |

- **采样率与 ring 需要重启**：引擎的时钟与 ring 在启动时定下（`live reload` 会明说这一点），
  但 `[stems]` 的默认值下一次分离就生效。未知键报错（拼错的 `sample_rat` 静默按 44.1k 跑才是灾难），
  越界的值被夹到有效范围并打印一行警告。
- **分开分离仍要求引擎率为 44.1 kHz**：内置模型是 44.1k 导出、缓存也按这个率做 key，
  非 44.1k 时 `stem separate` 直接报错而不是悄悄双向重采样。
- `[audio] block_frames` **不在这里**，因为它是引擎内部契约（时间拉伸引擎按块喂入），不是本机性能参数。
- **分离的块长也不在这里**：它就是模型自己的窗口步长（见 §3.7.1），设不了的才是对的。

### 3.12 profile 与 preference

```
profile/                 # 全部配置文件都在这里
├── profile.toml         # profile（--profile 不给时的默认）：只写"用哪几个文件"
├── mixer.toml           # 拓扑（= --config 的默认路径，也是 --print-config 打印的文本）
├── live.toml            # 本机设置（= --live 的默认路径）
├── midi.toml            # MIDI：设备名 + 映射文件
├── preference.toml      # 前端设置 —— 预留，目前不读
└── example.toml         # 命名 profile 的样例：复制成 <name>.toml 后 --profile <name>
```

profile 是**指路而不是又一套 schema**：拓扑、本机设置、MIDI 各自保有文件与校验，于是
`mixer reload` / `live reload` 仍然作用在它们被告知的那个文件上。
**文件里任何相对路径都相对"写它的那个文件所在的文件夹"** —— 所以 `profile/profile.toml` 里
`mixer = "mixer.toml"` 就是 `profile/mixer.toml`，命名 profile 和它指的文件放一起时不用写 `../`。

```toml
# profile/profile.toml
mixer = "mixer.toml"     # -> profile/mixer.toml
live  = "live.toml"      # -> profile/live.toml
midi  = "midi.toml"      # -> profile/midi.toml（也可以内联成 [midi] 表，见下）

[preference]             # 预留段：现在写进去的键会被**报告**，而不是被静默忽略

# 内联写法（与 midi.toml 的键相同）：
# [midi]
# device = "DDJ-400"     # 端口名字子串，或 `midi ports` 的序号；不给 = 启动时不打开输入
# map = "../midi-map.toml"   # 映射文件，相对本文件；不给则缺省 midi-map.toml（工作目录）
```

- 没有 profile（或 profile 里没写某项）时用文件夹默认：`profile/mixer.toml`、`profile/live.toml`、
  `profile/midi.toml`，各自缺失则回落内置默认（MIDI 缺失 = 不打开输入，即出厂状态）。
- **显式 flag 永远优先**：`--config` / `--live` / `--midi` / `--midi-map` > profile > 文件夹默认 > 内置默认。
- **切换 profile**：
  - 启动时 `--profile <name>`（裸名字 → `profile/<name>.toml`；`--profile path/x.toml` 直接给路径）；
    不给则读 `profile/profile.toml`（存在就用，不存在就是文件夹默认）。仓库里自带 `profile/example.toml`
    可复制成 `profile/dj.toml` 等处。
  - 会话内 `profile show` 看当前用的 profile 与它解析出的 topology/live/stems 参数；
    **`profile reload [<name|path>]` 切一个** —— profile 指到的 topology 走 `MixerReload`（立即生效）、
    live 立即生效（下一次分离就用新值），**只有 MIDI 是启动时接线的**，reload 会明说"下次启动生效"。
- 端口打不开时：**显式 `--midi` 直接报错退出**（你点名要的），**profile 里的设备只警告并继续**
  （"这套设备通常长这样"，控制器没插不该让整场起不来；TUI 里 `F2` 仍可临时选）。
- `--profile dj` 读 `profile/dj.toml`；`--profile path/to/p.toml` 直接读该路径。
- 段名拼错（`[mixr]`、`[midi] portt`）是错误，不是静默回落 —— 否则看起来用了某套配置其实没有。
- `[preference]` 是空段也合法（= 什么都没设）；`preference.toml` 目前不读，文件里写明了原因。

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
