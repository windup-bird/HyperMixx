# 波形后端：为什么 kitty graphics 在 Windows 上画不出波形

> 诊断结论（2026-11-02，WezTerm 20240203 on Windows）。本文只记录**问题定位**，修复方案见最后一节，
> 尚未落地到代码。

## 现象

`profile/preference.toml` 里 `backend = "auto"`，在 WezTerm 里启动 TUI：

- 响应日志打出 `[waveform] renderer kitty`（说明检测选了 kitty）；
- 波形区域**整块空白**：既没有图像，也没有 braille 兜底；
- 封面、overview 同样空（它们共用同一个后端判断）。

## 结论

不是 escape 序列写错了，而是**后端检测选错了**：

> Windows 上每个 pane 都隔着 **ConPTY**（`openconsole.exe`）。ConPTY 自己就是一个终端模拟器，
> 只维护文本网格再重新输出文本/CUP 之类的序列；APC 字符串（`ESC _ G … ESC \`，kitty graphics
> 的载体）在它的 `SosPmApcString` 状态里被直接丢弃，**永远不会转发给 WezTerm**。

所以 `a=t` / `a=p` 一个字节都没到终端，图片不可能出现；而 `render_kitty` 又把波形区域的字符格
全部设成 `CellDiffOption::Skip`，于是那块地方既没有图也没有字。

`auto` 之所以会选中 kitty，是因为 `kitty_hint()` 匹配的是环境变量（`WEZTERM_EXECUTABLE` /
`TERM_PROGRAM=WezTerm`），这些变量在 Windows 的 WezTerm pane 里照样存在，但它们只能说明
“**终端**是 WezTerm”，不能说明“**这条输出链路**上没有 ConPTY”。

## 证据

在本机一个真实 WezTerm 窗口里跑探针（`wezterm start -- ./probe.exe`，raw mode 下自己 `read` stdin
收回复），同一会话里发四种序列：

| 发送 | 终端回复 | 说明 |
|---|---|---|
| `CSI c`（DA1） | `ESC[?61;6;7;22;23;24;28;32;42c` | 这是 WezTerm 的 DA1，说明 CSI 通道通、回复能回来 |
| `CSI 6n`（DSR） | `ESC[1;1R` | 同上 |
| `ESC_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA`（kitty 支持性查询，`q=0` 应有 `OK`） | **无任何回复** | APC 没到 WezTerm；否则它会在 `KittyImage::Query` 分支回 `OK` |
| `a=t` 64×32 RGBA + `a=p`（全图 / `x,w` 源矩形裁剪 / `z=0` / `z=-1`） | **什么都没画出来** | 三种放置方式全灭，排除“只是源矩形或 z 序不对” |

附带发现：`crossterm::terminal::window_size()` 在 Windows 返回 `Err`（探针里记为 `window_size: err`，
`size()` 正常返回 `80x24`）。

另外核对了 WezTerm 源码（`wezterm-escape-parser/src/apc.rs` 的 `KittyImagePlacement::from_keys`、
`term/src/terminalstate/kitty.rs` 的 `kitty_img_place` / `assign_image_to_cells`、
`wezterm-gui/src/termwindow/render/screen_line.rs` 的 `z_index < 0` 分支）：`x/y/w/h`（源矩形）、
`c/r`、`p=`、`z=-1`（排在文字层之下、背景之上）、`C=1` 全部受支持。也就是说**我们发的报文本身没问题**，
换个能收到 APC 的终端就能画。

## 代码位置

| 位置 | 问题 |
|---|---|
| `crates/hypermixx-cli/src/tui/waveform_view/detect.rs:42` `resolve()` | 只挡了 `TMUX`/`STY`/`TERM=dumb`，没有 ConPTY（Windows）判断，`auto` 会照选 kitty |
| 同文件 `:94` `kitty_hint()` | `WEZTERM_EXECUTABLE` / `TERM_PROGRAM=WezTerm` 在 Windows 上同样成立；它描述的是“终端是谁”，不是“链路是否穿透” |
| 同文件 `:76` `cell_px()` | 依赖 `window_size()`，Windows 上恒失败 → 永远退回 8×16；即图形能画也会错位/留边（`preference.toml` 的 `waveform.cell` 就是为此留的手工覆盖） |
| `crates/hypermixx-cli/src/tui/waveform_view.rs` `render_kitty()` | 选中 kitty 后 `skip_area()` 把整块区域设成 `Skip`，图片没画出来时不会回退到 braille，所以表现为“空白”而不是“难看” |
| `crates/hypermixx-cli/src/tui/waveform_view/kitty.rs` | 报文正确（`a=t` 分块 + `a=p` 源矩形），无需改动 |

## 平台可行性

“Windows 上不行”的准确说法是“**中间隔着 ConPTY 就不行**”。kitty graphics 靠 APC 承载，而
ConPTY 的 VT 解析器对 SOS/PM/APC 的处理是写死的丢盘——`src/terminal/parser/stateMachine.cpp` 里
`_EnterSosPmApcString()` 调 `_ActionIgnore()`，`_EventSosPmApcString()` 的注释原文是
“In this state the entire string is ignored.”（DCS 反而有 `DcsPassThrough` 状态，这也是为什么 sixel
在 Windows Terminal 1.22+ 能通。）

| 环境 | kitty graphics | 原因 |
|---|---|---|
| Windows Terminal | ✗ | 本身不支持 kitty 协议（只支持 sixel），而且同样走 ConPTY |
| WezTerm（Windows，默认 ConPTY） | ✗ 已实测 | WezTerm 的 parser 支持整套协议，但序列在 ConPTY 就被吃掉了 |
| WezTerm + `wezterm ssh`（连 Linux/macOS 主机） | ✓ | pane 的 pty 在对端，输出不经 ConPTY |
| WSL / wsl.exe | ✗ | `wsl.exe` 作为 pane 进程仍然在 ConPTY 后面，绕不过去 |
| kitty / WezTerm / Ghostty / Konsole（Linux、macOS） | ✓ | 我们的报文本身没问题（已对照 WezTerm 源码） |
| tmux / screen | ✗ | 需要 `allow-passthrough`，当前 `resolve()` 直接回 braille |

结论：**原生 Windows 程序想画 kitty graphics，目前基本不可行**——不是 WezTerm 的锅，也不是我们序列的锅，
而是 Windows 上所有现代终端都通过 ConPTY 托管子进程。要么让 TUI 跑在 Linux/macOS（或 `wezterm ssh` 连过去），
要么在 Windows 上乖乖用 braille（这也是 `resolve()` 该做的判断）。

## 修复方案

### 方案 A：加 ConPTY 守卫（最小改动）

`resolve()` 里，`cfg!(windows)` 时把 `auto` 判为 braille，并给一条 note，例如
`"Windows 控制台经 ConPTY，APC 被丢弃；using braille"`。显式 `backend = "kitty"` 可选择沿用用户选择，
但同样记 note。缺点：Windows 上永远拿不到图形（ssh / mux 等真正直连的场景也一起被挡）。

### 方案 B：启动时做真实能力探测（推荐）

`detect.rs` 顶部注释本来就留了这条路（“A future round can add a DA1/Kitty query”）。做法：

1. `ratatui::try_init()` 之后、进入事件循环之前，raw mode 下发一条
   `ESC_Gi=<id>,s=1,v=1,a=q,t=d,f=24;AAAA ESC\`（`q` 缺省 = 详细模式，成功回 `ESC_Gi=<id>;OK ESC\`）；
2. 用**自己**的 `stdin().read()` 带 ~100ms 超时收回复 —— 不能用 `crossterm::event::poll/read`，
   它的解析器会把这条回复当杂音吞掉；
3. 读到 `OK` 才用 kitty，超时就用 braille（并把原因写进响应日志）。

这样 ConPTY、tmux、陌生终端都能自动判对，不需要 `cfg!(windows)` 特判。注意探测要在
`TryInit` 之后（raw mode 已开、屏幕还没画），并且回复读空之前不要进事件循环，否则残留字节会变成
假按键。

### 顺带

- `cell_px()` 在 Windows 上退化为 8×16 是既有事实，`waveform.cell` 是唯一手动补偿手段；
  如果做方案 B，探测得到“无像素尺寸”时可在日志里提示用户设 `cell`。
- 修好后若仍想支持“Windows 上也能看到图”，可行的方向是走 WezTerm/iTerm2 的 `OSC 1337;File`
  （`wezterm imgcat` 用的就是它，ConPTY 能过），但它没有源矩形/放置语义，滚动波形每帧都要整张重传，
  只适合封面这类静止图，不适合波形。

## 如何复现这份诊断

```bash
# 任意最小 probe：raw mode → 发查询 → 读 stdin
printf '\033_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\033\\'   # 有回复 = 能走 kitty graphics
```

在 WezTerm 的 Windows pane 里（ConPTY）它不会回任何东西；在 Linux/macOS 的 kitty、WezTerm、
Ghostty 里会回 `ESC_G…;OK ESC\`。这也是方案 B 的判据。
