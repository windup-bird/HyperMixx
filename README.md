# Hypermixx

基于Rust的跨平台混音软件，目前仅支持cli/tui。

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
| `hypermixx-core` | 协议与类型：`Command`/`CommandResponse`、`DeckState`、`BeatGrid`、`Key`、`Source` | serde |
| `hypermixx-media` | 解码与内存池：`decode_file`（symphonia → 44.1k 立体声）、`PcmPool` | core |
| `hypermixx-audio` | 实时引擎：producer 线程 + cpal 输出、mixer/通道/FX 链、时间拉伸 | core, media |
| `hypermixx-library` | 离线分析：beat 网格编译、stratum-dsp 适配、波形峰值 | core, media |
| `hypermixx-stems` | 离线 stem 分离：HTDemucs 模型获取/校验、内容寻址缓存、ONNX 后端（CPU 默认；`cuda` 可选） | core, media, ort |
| `hypermixx-midi` | MIDI 输入：字节解析、TOML 映射表、`Event → Command` 翻译（纯逻辑，可无硬件单测） | core, midir |
| `hypermixx-cli` | 前端：行 REPL、`--tui` 终端界面、命令解析与补全 | 全部 |

外部依赖：`stratum-dsp`、`timestretch`、`midir`、`charon-audio`/`ort`（仅 `hypermixx-stems` 的
`onnx` feature，默认开）。`vendor/charon-audio` 是带了 15 行补丁的副本 —— 上游 0.1.2 的
`ExecutionProvider` 只有 CPU/CoreML，补丁加上 CUDA 一档，见 `vendor/charon-audio/PATCH.md`。


## 安装

仅在Linux完成测试。

前置：Rust（2021 edition，1.70+）、音频输出设备；Linux 还需 ALSA 开发库。

```bash
sudo apt install libasound2-dev   # Debian/Ubuntu

git clone <repo> && cd HyperMixx
cargo build --release
cargo build --release --features cuda  # use cuda to separate stems
```

## 示例操作

```bash
cargo run -p hypermixx-cli --features cuda - --tui
```

```text
deck0> load test.mp3 122
deck0 loaded: 18462369 frames (7:00.648)

deck0> play                        # 切换播放/暂停（toggle）
deck0> cue                         # smart：播放中回 cue 并暂停；暂停时把当前帧记为 cue 点
deck0> cue set                     # 显式记 cue 点
deck0> tempo 1.04                  # 设 tempo（1.0 = 原速）
deck0> tempofader 0.5              # 推子位置（tempo = 1 + pos × temporange）
deck0> temporange 0.16             # 推子满量程 ±16%（缺省 0.1）
deck0> keylock wide                # 变调引擎：on（默认）| off | wide
deck0> key 1                       # 变调半音（占位：只记录，暂不发声）
deck0> beatjump 16
deck0> fx list                    # 列出fx
deck0> fx set eq low -0.5         # -1~1
deck0> fx set filter value  0.5
deck0> master fx list

# stems：离线分离 4 条轨，然后逐条控制（分离在后台跑，期间照常 play/loop/jump/sync）
deck0> deck0 stem separate         # CPU：首次 ~87s（3:39 曲目）+ ~2GB 内存；之后命中缓存 0s
deck0> deck0 stem separate --gpu   # CUDA：13s（同曲目，RTX 4050）；需 --features cuda 构建
deck0> deck0 stem separate --overlap 0.0   # 0.25→0.0：少 24% 时间，窗口边缘权重更大
deck0> deck0 stem acapella         # 只要人声（instrumental | drums | bass | full）
deck0> deck0 vocals level -0.5     # 单条 stem 电平（-1 = 精确静音）
deck0> deck0 vocals mute           # solo 仍然优先
deck0> deck0 vocals fx add filter  # 只给人声加滤波（deck0 fx … 则是整 deck 共用）
deck0> deck0 stem status           # 逐 stem 电平/静音/独奏

# 拍同步：deck1 对 deck0
deck0> deck1 sync phase pid        # 先对 BPM，再用 PI 追相位（pll 收敛后归零）
deck0> deck1 sync tempolock        # 两边共享一个 tempo，任一边 fader 都带动对方
deck0> deck0 sync set-leader       # 指定 master（target 即 leader，无参数）
deck0> deck1 nudge 0.04 0.5        # 临时 +4% 挪相位，0.5s 后自己松开（tempo 不变）
deck0> deck1 sync unlock           # 解锁：清 lock/align/nudgerate，tempo 保留
deck0> quit
```

启动参数：

```bash
cargo run -p hypermixx-cli -- --backend auto        # 分析后端 auto | stratum | timestretch
cargo run -p hypermixx-cli -- --config topo.toml    # 自定义拓扑
cargo run -p hypermixx-cli -- --print-config        # 打印参考 TOML
cargo run -p hypermixx-cli -- --midi 0              # 打开 0 号 MIDI 输入，用默认 midi-map.toml
cargo run -p hypermixx-cli -- --midi 0 --midi-map my.toml   # 指定映射文件
cargo run -p hypermixx-cli -- --midi-guide          # learn 模式编辑映射（不启动引擎，端口/文件在 TUI 里选）
```

TUI 内尽量不用启动参数:`--tui` 下按 `F2` 选 MIDI 端口、`F3` 选映射文件；`load` 不带路径则弹出
文件浏览器；`--midi-guide` 即使不跟路径也会在 TUI 里选端口与文件。

终端内 `midi ports` 列出可用 MIDI 输入端口；映射表格式与全部 action 见 `midi-map.toml` 注释与 [`docs/midi-mapping.md`](docs/midi-mapping.md)。


## 脚本

`./scripts/phase_probe.sh`: 6次beatjump测试相位差
`./scripts/sync_phase.sh`: 同曲先后播放sync测稳态相位差

## Stem Separate

`stem separate` 把当前曲目离线分离成[drums/bass/other/vocals]并**热装**进 deck，pcm缓存在`~/.config/hypermixx/stems/...`。

使用`--features cuda`构建并在cli使用`stem separate --gpu`可以用cudnn加速。


## Vinyl

目前仅简单实现，`timestretch`目前似乎没法负速度播放。

## todo

1. midi control: nearly done
2. better stem/stemfx
3. network streamming
4. better analysis
5. better vinyl

## 许可证

MIT
