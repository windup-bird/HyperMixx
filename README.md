# Hypermixx

基于Rust的跨平台混音软件，目前仅支持 cli/tui。直接运行进入 TUI，`--cli` 进入纯命令行，`--guide` 进入 controller-guide。
旧的 `--tui` / `--midi` / `--midi-map` / `--midi-guide` 拼写已移除。

## 架构

分层单向依赖，下层不感知上层：

```
cli ─┬─► audio ───┐
     ├─► library ─┼─► media ──► core
     ├─► stems ───┤
     ├─► midi ────┤
     └─► core ────┘
```

| crate | 职责 | 依赖 |
|---|---|---|
| `hypermixx-core` | 协议与类型：`Command`/`CommandResponse`、`DeckState`、`BeatGrid`、`Key`、`Source`、action 语法表、live 设置、输出状态 | serde |
| `hypermixx-media` | 解码与内存池：`decode_file`（symphonia → 44.1k 立体声）、`PcmPool` | core |
| `hypermixx-audio` | 实时引擎：producer 线程 + cpal 输出、mixer/通道/FX 链、时间拉伸 | core, media |
| `hypermixx-library` | 离线分析：beat 网格编译、stratum-dsp 适配、波形峰值 | core, media |
| `hypermixx-stems` | 离线 stem 分离：HTDemucs 模型获取/校验、内容寻址缓存、ONNX 后端（CPU 默认；`cuda` 可选） | core, media, ort |
| `hypermixx-midi` | MIDI 输入：字节解析、TOML 映射表、`Event → Command` 翻译（纯逻辑，可无硬件单测） | core, midir |
| `hypermixx-cli` | 前端：默认 TUI（`--cli` 切行 REPL）、命令解析与补全 | 全部 |

外部依赖：`stratum-dsp`、`signalsmith-stretch`(vendored)、`midir`、`charon-audio-cuda`(forked from `charon-audio`)

## 安装

Linux 与 Windows（MSVC）均已完整构建并通过测试。

前置：Rust（2021 edition，1.70+）、音频输出设备。

Linux 还需 ALSA 开发库：

```bash
sudo apt install libasound2-dev   # Debian/Ubuntu

git clone <repo> && cd HyperMixx
cargo build --release
cargo build --release --features cuda  # use cuda to separate stems
```

Windows 还需要两个原生依赖：

- **OpenSSL**：`hypermixx-library` 的 `bundled-sqlcipher` 需要头文件和 `libcrypto.lib`。
- **LLVM/libclang**：vendored `signalsmith-stretch` 的 `build.rs` 用 bindgen 生成绑定。

```powershell
winget install ShiningLight.OpenSSL.Dev LLVM.LLVM

$env:OPENSSL_LIB_DIR    = "C:\Program Files\OpenSSL-Win64\lib\VC\x64\MD"
$env:OPENSSL_INCLUDE_DIR = "C:\Program Files\OpenSSL-Win64\include"
$env:LIBCLANG_PATH       = "C:\Program Files\LLVM\bin"
$env:PATH                = "C:\Program Files\OpenSSL-Win64\bin;$env:PATH"  # 运行期找 libcrypto-*-x64.dll

git clone <repo>; cd HyperMixx
cargo build --release
cargo build --release --features cuda  # 需 CUDA 13 + cuDNN 9
```

把上面四个环境变量写进用户环境变量（或 `.cargo/config.toml` 的 `[env]`）就不必每次设置。

## 示例操作

```bash
cargo run -p hypermixx-cli                      # 默认就是 TUI
cargo run -p hypermixx-cli -- --cli             # 纯命令行（REPL）
```

```text
deck1> load test.mp3 122
deck1 loaded: 18462369 frames (7:00.648)

deck1> play                        # 切换播放/暂停（toggle）
deck1> cue                         # smart：播放中回 cue 并暂停；暂停时把当前帧记为 cue 点
deck1> cue set                     # 显式记 cue 点
deck1> tempo 1.04                  # 设 tempo（1.0 = 原速）
deck1> fader flow -0.2             # 通道/逐流电平（只衰减，顶端 0 dB）
deck1> fader gain 1.0              # 通道 trim：0 = 0 dB，1 = +6 dB
deck1> fader cross -1.0            # crossfader（共用；位置与曲线见 [crossfader]）
deck1> tempofader 0.5              # 推子位置（tempo = 1 + pos × temporange）
deck1> temporange 0.16             # 推子满量程 ±16%（缺省 0.1）
deck1> keylock on                  # 变调引擎：on（默认）| off；窗口在 live.toml 的 [keylock]
deck1> key 1                       # 变调半音（占位：只记录，暂不发声）
deck1> beatjump 16
deck1> fx list                    # 列出fx
deck1> fx set eq low -0.5         # -1~1
deck1> fx set filter value  0.5
deck1> master fx list

# stems：离线分离 4 条轨，然后逐条控制（分离在后台跑，期间照常 play/loop/jump/sync）
deck1> stem separate         # 分段分离：先装四路流（此刻仍听原曲），每 5.85s 一块落地即可用
deck1> stem status           # 水位：`25% separated`（未装入部分是原曲回落）
deck1> stem separate --model htdemucs-fp16   # 临时换模型（provider/overlap/shifts 在 live.toml）
# CUDA：在 live.toml 设 `[stems] provider = "cuda"`，需 --features cuda 构建（13s，RTX 4050）
# 更快：设 `[stems] overlap = 0.0` 后 `live reload`，分离时间少约 24%，窗口边缘权重更大
deck1> stem acapella         # 只要人声（instrumental | drums | bass | full）
deck1> vocals level -0.5     # 单条 stem 电平（-1 = 精确静音）
deck1> vocals mute           # solo 仍然优先
deck1> vocals fx add filter  # 只给人声加滤波（deck1 fx … 则是整 deck 共用）
deck1> stem status           # 逐 stem 电平/静音/独奏

# 拍同步：deck2 对 deck1
deck1> deck2 sync phase pid        # 先对 BPM，再用 PI 追相位（pll 收敛后归零）
deck1> deck2 sync tempolock        # 两边共享一个 tempo，任一边 fader 都带动对方
deck1> sync set-leader       # 指定 master（target 即 leader，无参数）
deck1> deck2 nudge 0.04 0.5        # 临时 +4% 挪相位，0.5s 后自己松开（tempo 不变）
deck1> deck2 sync unlock           # 解锁：清 lock/align/nudgerate，tempo 保留
deck1> quit
```

启动参数：

```bash
cargo run -p hypermixx-cli -- --cli                  # 纯 REPL（默认是 TUI）
cargo run -p hypermixx-cli -- --guide                # controller-guide（不启动引擎）
cargo run -p hypermixx-cli -- --backend auto        # 分析后端 auto | stratum | timestretch
cargo run -p hypermixx-cli -- --config topo.toml    # 自定义拓扑
cargo run -p hypermixx-cli -- --print-config        # 打印参考 TOML（= profile/mixer.toml）
cargo run -p hypermixx-cli -- --profile dj          # 用 profile/dj.toml 指路（拓扑 + 本机设置 + MIDI 设备）
#   会话内切换：`profile show` 看当前 profile，`profile reload dj` 换一套（控制器下次启动生效）
cargo run -p hypermixx-cli -- --live live.toml      # 引擎采样率 / ring 大小 / keylock 窗口 / stem 默认值
cargo run -p hypermixx-cli -- --controller 0       # 打开 0 号控制器输入，用 profile/controller.toml
cargo run -p hypermixx-cli -- --controller 0 --controller-map my.toml   # 指定映射文件
cargo run -p hypermixx-cli -- --guide              # learn 模式编辑映射（不启动引擎，端口/文件在 TUI 里选）
#   旧的 --tui / --midi / --midi-map / --midi-guide 已移除
```

TUI 内尽量不用启动参数:`F9` 打开设置（控制器端口、映射文件、mixer 输出设备都在这里选，取代了旧的 F2/F3）；
`load` 不带路径则弹出文件浏览器；`--guide` 即使不跟路径也会在 TUI 里选端口与文件。波形左右声道上下镜像，三频段颜色、
分频点（`low_hz`/`high_hz`，内置默认 200/4000Hz，随附文件用 250/4000Hz）、渲染 backend 与图像刷新率在 `profile/preference.toml` 配置：
`auto` 按 kitty → braille 选择（tmux/screen 暂回 braille）；kitty 可用 60Hz。
Windows 上经 ConPTY 时 kitty graphics 的 APC 序列会被丢掉（表现为选中 kitty 但波形区空白），
原因与修法见 [`docs/WAVEFORM.md`](docs/WAVEFORM.md)。

### TUI 布局

```
header（状态 + ⚙ settings）
┌ Deck1 · 标题 ─────────────┐┌ Deck2 · 标题 ─────────────┐
│ cover  状态行              ││ cover  状态行              │
│        waveform-overview  ││        waveform-overview  │
└───────────────────────────┘└───────────────────────────┘
waveform（每 deck 2 行，高终端 ≥34 行时 3 行；无边框无背景）
┌ 1-pads ───┐┌ 1-beatctl ┐┌ 1-eq ──┐┌ faders ────────┐┌ 2-eq ──┐┌ 2-beatctl ┐┌ 2-pads ───┐
│ 2×4 pad   ││ tempo     ││ 5 knobs ││fader │meter│ f││ knobs  ││ tempo     ││ 2×4 pad   │
│ 每块 3 行 ││ range     ││ sync key││                 ││        ││ range     ││ 每块 3 行 │
│ cue/loop/jump             │ bend      ││ play cue│（空一行）       ││        ││ bend      ││
└───────────┘└───────────┘└─────────┘│crossfader ░░░░░│└────────┘└───────────┘└───────────┘
                                     └────────────────┘
browser | command
```

- 每个 deck 的控件带按 **N-pads : N-beatctl : (N-eq + faders) = 1:1:1** 平分（面板标题带碟盘编号）；
  `trans`（sync/key、play/cue 两行）放在 eq 面板的旋钮下方，这样三块面板的底部按钮行对齐，也不用再挤一列。
- eq 的 5 个横条用 `▬`（居中矩形，不占满整行）画，空的部分也是 `▬`、只差颜色：这样每条上下
  自带一点空隙，`filt` 不会贴住下面的 transport 按钮，也不需要为间隙额外加整行。
- pads 是 2 行 4 列，每块 3 行；pads/beatctl 底部那行只放模式按钮（cue/loop/jump、bpm/loop/jump），
  按钮内文字居中，没有别的文字；tempo 的读数在上、`range ±N%` 正好在它下方。
- 两个 deck 的 fader 和它们的电平条**同框**（中间那个 `faders` 框）：**左列是 deck2 的电平、右列是 deck2 的电平**
  （不是 master/cue，那对数值仍在 header 的 `out`/`cue` 里）。crossfader 画在这个框的**最底下一行**，
  与上面空一行；框内横向间隙统一 2 列，与那 1 行空白（约等于 2 列宽）视觉一致。
- fader 的宽度是 **2 列**，正好等于 crossfader 那 1 行的像素厚度（字符格约 1:2），所以竖着的 deck fader
  和横着的 crossfader 看起来是同一根推子。电平条各 1 列，分居框的中线两侧，让 crossfader 的中点落在中线上。
- crossfader 用 fader 的样式（`█` 实心 / `░` 空格），但它是**双极**的：中间 0 位时两边都不填充，
  向左/向右才从中点往外填。
- 启动时焦点在 **browser**：方向键直接浏览曲库。browser 是"软焦点"——它只拿走方向键，命令行的 deck
  命令仍然落到最近聚焦的 deck（默认 deck1），不用先按 `Tab`。
- 宽度 < 112 或高度 < 26 时整条控件带隐藏，只留 header + deck 信息 + 波形 + browser/command；宽度不到 120 时
  mixer 旋钮自动省掉数值，只留长条。
- 控件全部可用鼠标点：pads 三档、beat 三档、fader/knob/crossfader 按位置取值。键盘仍然只属于命令行
  （`Tab` 切目标，`F9` 设置）。
- 颜色集中在 `preference.toml` 的 `[theme]`（`accent`/`dim`/`text`/`error`/`warn`/`pad`，`#rrggbb`）。
- 封面与 overview 和波形共用渲染后端：kitty 下都走 graphics 协议（同一个 image/placement 生命周期，
  弹层打开时不发送也不放置，关闭后重传重放，避免残留），其它终端下封面用半格真彩、overview 用 braille。

`profile/` 放全部默认文件：`mixer.toml`（拓扑，含 `[[output]]` 输出设备）、
`live.toml`（本机设置）、`controller.toml`（映射文件，设备名在 `[meta].device`；旧的 `midi.toml` 仍会被读）、
`preference.toml`（主题/波形/曲库）。命名 profile 也放这里（`example.toml` 是样例；`--profile dj` →
`profile/dj.toml`，不给则找 `profile/profile.toml`，没有就用文件夹默认）。文件里的相对路径都相对它所在的文件夹。
运行时改配置不必重启：`mixer reload` 重建拓扑（保留走带），`live reload` 重读本机设置
（引擎率/ring 需重启，`[stems]` 立即生效）。

终端内 `controller ports` 列出可用输入端口（`midi ports` 仍可）；映射表格式与全部 action 见
`profile/controller.toml` 注释与 [`docs/MIDI.md`](docs/MIDI.md)。

`F9`（或 header 右侧的 settings 按钮）打开设置界面——**它不是 TOML 编辑器**，只有四项：

```
> controller   Numark Party Mix III · profile/controller.toml
  mixer        profile/mixer.toml · 2 output(s)
  live         profile/live.toml
  preference   profile/preference.toml
```

`→`/`Enter` 进下一层，`←` 退回上一层，`Esc` 关闭；每一层都是同一个**单栏平铺**列表（和 `--guide` 里的弹窗完全一样，
没有树、没有双栏、没有第二个窗口）：

- `controller`：先进 `controller › input`（MIDI 端口列表），选定后自动进入 `controller › map`（`.toml` 文件选择），
  选定即重连。文件选择里 `→`/`Enter` 进入目录、`⌦` 上一级目录、`←` 退回上一层。
- `mixer`：`mixer`（每条 `[[output]]` 一行）→ `… › input`（hardware）→ `… › channels`（channel pair），
  选定即写 `mixer.toml` 的 `device`/`channels` 并重载拓扑。`●` 当前绑定、`◆` 本 controller 的声卡、
  `✓/✗ rate` 采样率。立体声设备没有 channel 这一层。
- `live`、`preference`：选新的 `.toml`；`live` 立即按 `live reload` 重新读取（引擎率/ring 需重启），
  `preference` 立即换主题。
- 其余按键：`↑↓` 移动、`r` 重读 mixer 文件（设备列表只在启动前探测一次，避免 ALSA 警告糊到界面上）。

弹窗出现时 deck 上的 kitty 图片会被立即删除（kitty 图片在文字层之下，只是不再绘制的话会从弹窗里透出来），
关闭后自动重传。

## 脚本

`./scripts/phase_probe.sh`: 6次beatjump测试相位差
`./scripts/sync_phase.sh`: 同曲先后播放sync测稳态相位差

## 曲库

`library` 读取本地文件夹和 rekordbox / VirtualDJ / Serato 的 USB 导出，缓存到
`$HYPERMIXX_CACHE/library/library.sqlite3`（默认 `.cache/library/`）。U 盘只读，不写回。

```toml
# profile/preference.toml
[library]
local = ["rkb", "serato", "vdj"]   # 本机用户目录里的库
usb   = ["rkb", "serato", "vdj"]   # 插拔 U 盘时自动挂载并发现的库
folders = ["~/Music"]              # 普通文件夹；`~` 展开为用户主目录源
```

```text
browser refresh                       # 增量重扫已配置来源
browser list                          # 来源和 id
browser tracks <list-id>              # list 内容
browser show <location-id>            # 单曲详情
```

启动时 browser 优先打开 `~/Music` 来源；没有这个来源时停在第一个已配置来源。
扫描在后台进行，界面先显示缓存，完成后自动刷新。未出现在 `local` / `usb` / `folders`
里的来源不会扫描，也不会显示。同一个 U 盘的
rekordbox / VirtualDJ / Serato 会作为三个不同 library 出现。USB 插拔由后台 watcher
轮询 `/sys/block`，未挂载时调用 `udisksctl mount --no-user-interaction`。

- rekordbox：优先 `exportLibrary.db`（SQLCipher），没有时回退 `export.pdb`；ANLZ 的
  `PQTZ`/`PCO2` 导入 beatgrid 和 hot cue。
- VirtualDJ：`database.xml` + `vdj-export/vdj-list.m3u`，有 ANLZ 时一并导入。
- Serato：`database V2` + `Subcrates/*.crate`，cue/grid 从 ID3 `GEOB` 读取。
- 同一音频按内容哈希合并；路径只属于 location。
- RB 的 ANLZ 时间保留原始值，并在 cue/grid 行记录 `offset_ms`。实测以 RB/VDJ 为 0 时，
  Serato tag 晚约 16.5ms，因此默认 `serato_offset_ms = -16.5`。
  加载时先减掉解码器 gapless 起点（MP3 样本 25.0567ms），再加可配置的微调，最后换算到帧。
- 默认 RB 口令是公开常量；需要覆盖时设 `HYPERMIXX_REKORDBOX_KEY`。
- `profile/preference.toml` 的 `[library] local|usb|folders` 决定启动时扫描/显示哪些来源。
  `decoder_start_ms`、`grid_offset_ms` 以及 `rkb_mp3_offset_ms` / `serato_offset_ms` /
  `vdj_offset_ms` 控制导入网格的时间补偿。`[display] key = "camelot"`（默认）可改为 `"classic"`。

TUI：`Tab` 到 browser 后，`↑/↓` 选择、`→/Enter` 打开（最低一级进入 tracks）、`←/Esc` 返回；
track 上 `Enter`/`load [deck]` 加载，`analyse` 分析并缓存；鼠标双击 track 弹出 deck 选择框（1/2，
默认停在第一个；`browser load 2` 这种显式指定的会预选对应行）。
MIDI 可用 `browser.up/down/open/exit/load/refresh`，`hotcue set|trigger|clear <1..8>` 控制 hot cue。

## Stem Separate

`stem separate` 把当前曲目离线分离成[drums/bass/other/vocals]，pcm缓存在`./.cache/stems/...`（工作目录下的 `.cache`，可用 `HYPERMIXX_CACHE` 覆盖）。

## todo

1. midi control: nearly done
2. better stem/stemfx
3. network streamming
4. better analysis

## 许可证

MIT
