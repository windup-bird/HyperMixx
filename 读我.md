# HyperMixx

> NO AIGC HERE

基于Rust的跨平台DJ混音软件，豪情在天的终端界面(Ratatui)，灵活易用的映射编辑，高性能和低占用。

未来计划扩展Flutter前端和Android支持。

## 特性

### 混音链路

```text
PCM -> flows(stems) -> stretch(keylock) -> prefader -> eq -> filter flowfx -> deckfx -> postfader -> limiter -> outputs
```

可灵活配置cue监听/fader于fx的位置(pre/post)，fx可分stem应用并灵活映射到你的控制器，可选limiter和autogain。

时间拉伸算法使用`SignalSmith`(MIT)。

### 读取Rekordbox/Serato/VDJ曲库

插上U盘，或者在配置里指一个文件夹，剩下的交给它。

- **rekordbox**：优先读 `exportLibrary.db`（SQLCipher，口令是公开常量，需要时可用 `HYPERMIXX_REKORDBOX_KEY` 覆盖），没有就退回 `export.pdb`；ANLZ 里的 beatgrid 和 hot cue 一起带走。
- **Serato**：读 `database V2` 和 `Subcrates/*.crate`，cue/grid 从音频文件的 ID3 `GEOB` 里取。
- **VirtualDJ**：读 `database.xml` 和 `vdj-export/vdj-list.m3u`，有 ANLZ 也一起导入。
- 普通文件夹也认，`~` 会展开成你的主目录。
- 同一个音频按内容哈希合并，换路径、换块盘不会变成两首；U 盘只读，绝不写回。
- 扫描在后台跑，界面先显示缓存，扫完自己刷新。缓存是一份可重建的 sqlite（`$HYPERMIXX_CACHE/library/`，默认 `.cache/library/`），删了最多重扫一次。

加载时优先用导入的 grid，其次本地分析缓存，最后才看标签 BPM；`[preference.analysis] auto_analyse = true` 时，前两者都没有才在后台自动分析。导入时间会补偿解码器的 gapless 起点和各家的偏移（默认 Serato −16.5 ms），所以网格落点是对得上的。key 默认 Camelot（8A/8B），想换成 Am/C 改 `[preference.display] key = "classic"`。

### 两分钟内驱动你的设备

```text
Midi-Message -> Slot -> Unit -> Command
```

Slot是控制器上每个元件(按钮?旋钮?转盘?)的类型抽象，Unit是控制方式的抽象集合，每个Unit可定义映射组合(类似padmode/stemfx)并定义切换方式。

提供灵活易用的映射向导，每个Deck可定义多个Unit，并一键克隆到另一组Deck元件。

映射不用从零写：内置了 `mixer`、`transport`、`jog`、`pad`、`browser` 这些常用 preset，插上控制器先跑起来，缺什么再进去学。所有东西最后都落在一个能读能改的 TOML 里，向导里够不着的细节（action 参数、曲线、值域）也可以手改。

### 流式Stem分离

基于`Charon`(MIT)，加载HTDemucs模型，滚动窗口分离并在完成后流式装载，完成高质量的实时分离。

四路 stem（drums/bass/other/vocals）不是等整首分完才出现：装进 deck 的那一刻和原曲完全一样，之后模型每推进一个窗口就落地一块，播放中就能逐条推电平、静音、加效果。分离在后台跑，期间 play/loop/jump/sync 一切照旧。

默认 CPU 分一首 3:39 约 87 秒；`[live.stems] provider = "cuda"` 后约 13 秒（RTX 4050）。结果按内容哈希缓存，同一首再跑 0 秒，一首约 300 MB，`stem cache prune` 清旧缓存。

Provider可选cpu/cuda，启用cuda加速特性：
```bash
cargo run --release --features cuda
```

## 安装

在Linux开发，Linux/Windows完成测试。

需要 Rust stable 和一块能出声的声卡。第一次构建会拉 ONNX Runtime 的预编译包（CPU 约 101 MB），之后只重建本仓库。

### Linux

除了 Rust，还要 ALSA 开发库：

```bash
sudo apt install libasound2-dev   # Debian/Ubuntu

git clone https://github.com/windup-bird/HyperMixx
cd HyperMixx
cargo build --release --workspace
cargo run --release
```

### Windows

需要三样原生依赖，缺任何一样都是**构建期直接失败**：

| 依赖 | 干什么 | 装法 |
|---|---|---|
| MSVC C++ 工具链 | 所有 native 依赖 + vendored 的 `signalsmith-stretch` | VS Installer 勾选「使用 C++ 的桌面开发」 |
| OpenSSL（头文件 + `libcrypto.lib`） | 曲库的 `rusqlite/bundled-sqlcipher` | `winget install ShiningLight.OpenSSL.Dev` |
| LLVM / libclang | vendored 依赖的 bindgen | `winget install LLVM.LLVM` |

装到默认位置后，在当前 PowerShell 会话里设好路径（`OPENSSL_LIB_DIR` 必须指到 `lib\VC\x64\MD`，不是 `lib`）：

```powershell
$env:OPENSSL_LIB_DIR     = "C:\Program Files\OpenSSL-Win64\lib\VC\x64\MD"
$env:OPENSSL_INCLUDE_DIR = "C:\Program Files\OpenSSL-Win64\include"
$env:LIBCLANG_PATH       = "C:\Program Files\LLVM\bin"

cargo build --release
.\target\release\hypermixx-cli.exe
```

运行期还要让 `libcrypto-*-x64.dll` 找得到：把 `C:\Program Files\OpenSSL-Win64\bin` 留在 `PATH` 上，或者把 DLL 拷到 exe 旁边。完整的报错对照表和 CUDA 依赖见 [README.md](README.md) 的 Windows 一节。

## 开始混音

第一次启动会自己在默认位置生成一份 `config.toml`（Linux 是 `~/.config/hypermixx/`，Windows 是 `%APPDATA%/Hypermixx/`），不用先写配置。默认进 TUI：方向键在曲库里走，`Enter` 打开/加载，`Tab` 把焦点交给 deck，`F9` 打开设置。

### 载入曲库

曲库来源都在 `config.toml` 的 `[preference.library]` 里，没列出来的类别不扫描、也不显示。

#### 本地曲库

```toml
[preference.library]
local   = ["rkb", "serato", "vdj"]   # 用户目录里的软件曲库
folders = ["~/Music"]                # 普通文件夹，`~` 会展开
```

启动时 browser 优先打开 `~/Music`。想立刻重扫，命令行敲 `browser refresh`，或者用 `browser list` 看现在有哪些来源、上次什么时候扫的。

#### USB曲库

```toml
[preference.library]
usb = ["rkb", "serato", "vdj"]
```

插上就认。后台会轮询 `/sys/block` 找可移动盘，没挂载就调 `udisksctl mount --no-user-interaction` 挂上，然后只读地读一遍。同一块 U 盘上的 rekordbox / Serato / VirtualDJ 会作为三个不同的 library 出现，互不打架。

### 使用命令行

默认是 TUI；想用纯命令行加 `--cli`。两边是同一套语法，行首可以点名叫哪个 deck：

```bash
cargo run --release -- --cli
```

```text
deck1 load ~/Music/shelter.mp3 122
deck1 play
deck1 cue
deck1 beatjump 16
deck1 loop 8
deck1 fx add echo
deck1 stem separate

deck2 load ~/Music/nepture.mp3
deck2 sync phaselock pid  # 先对 BPM，再用 PI 持续追相位
deck2 sync tempolock      # 两边共享一个 tempo
deck1 sync set-leader     # 指定 master
deck2 nudge 0.04 0.5      # 临时 +4%，半秒后自动松开
```

裸命令（`play`、`loop 4`）作用于当前焦点 deck；只有 `master fx …` 走总线。`help` 看命令总表，`quit` 退出。常用动词不多：`load`、`play`、`cue`、`loop`、`beatjump`、`sync`、`fx`、`stem`，具体到每个参数看 [docs/COMMANDS.md](docs/COMMANDS.md)。

### 连接碟机

标准 MIDI 控制器插上就能用，不需要装驱动。

- `controller ports` 列出可用端口；`--controller 0` 启动时直接打开 0 号，或者进 TUI 按 `F9` 在设置里选。
- 映射文件由 `config.toml` 的 `[controller] map = "mapping.toml"` 指定，路径相对配置文件所在目录。
- 配置里写了端口但设备没插，不会拦着你启动；插上以后 `F9` 里再选一次就行。
- 同一个映射文件可以给多台同型号控制器用，端口在设置里换，映射不用重写。

## 编辑映射

启动映射向导：

```bash
cargo run --release -- --guide
```

向导不启动音频引擎，改映射的时候不会突然出声。界面很直白：左边是 `deck | unit | slot` 三栏，右边是说明和 watch dog（看 MIDI raw、看报错），底部一排按钮。

- `Enter` 往下钻；`new` 可以 `clone from deck1`，也可以从内置 preset 里挑一个新建 unit。
- 到 slot 上按 `Enter` 进 Learn，动一下控制器上的实物，再 `Enter` 确认。14-bit 的旋钮会自动认出来，不用自己算 CC 号。
- 一个 unit 可以有几套 mode，按模式键切换；`mode_trigger` 直达某一套，`mode_cycle` 按顺序轮。
- 学完 `Save`，存成普通的 TOML。action 的参数就写在行内（`loop 1/4`、`nudge 0.15 0.06`），曲线和值域也能手改。
- 仓库里的 `profile/mapping.toml` 是一份带注释的完整样例；schema 和全部 action 见 [docs/MIDI.md](docs/MIDI.md)。

## 配置

只有一个文件，分四段：

```toml
[mixer]        # 拓扑：通道、crossfader、master limiter、输出设备
[live]         # 这台机器：采样率、ring、keylock、stem、autogain
[preference]   # 界面口味：波形、主题、曲库、key 显示、分析后端
[controller]   # 控制器：map 文件指针，可选 device
```

不给 `--config` 就用平台默认路径（Linux `$XDG_CONFIG_HOME/hypermixx/config.toml`，Windows `%APPDATA%/Hypermixx/config.toml`）；`HYPERMIXX_CONFIG` 可以覆盖，`--print-config` 打印内置模板。仓库里的 `profile/config.toml` 就是这份模板。

改完不必重启：

- `config reload` 重读整份配置。
- `mixer reload [file]` 就地重建拓扑，正在放的歌不会停；文件里没写的键回到默认值，所以想保留的推子/trim 要一起写上。
- `live reload` 重读本机设置。采样率和 ring 要重启才生效，`[live.stems]` 下一首分离立刻用新值。
- 输出设备可以在 `F9` 里选，也可以 `output list` 看状态、`output refresh` 重试没插好的设备。

段名或键名拼错会直接报错，不会偷偷按默认值跑——演出中途「看起来用了某套配置其实没有」比报错难查得多。

## 计划中的功能

1. 基于Flutter的GUI
2. Android支持，包括ratadroid/flutter
3. 网络/流媒体串流
4. 更好的网格分析算法
5. 脚本控制和SetMaker
6. 基于LLM的自动混音
7. 局域网互联和Sync

## 许可证

MIT
