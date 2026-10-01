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
hypermixx> load test.mp3 122
deck0 loaded: 18462369 frames (7:00.648)

hypermixx> play                        # 切换播放/暂停（toggle）
hypermixx> cue                         # smart：播放中回 cue 并暂停；暂停时把当前帧记为 cue 点
hypermixx> cue set                     # 显式记 cue 点
hypermixx> tempo 1.04                  # 设 tempo（1.0 = 原速）
hypermixx> tempofader 0.5              # 推子位置（tempo = 1 + pos × temporange）
hypermixx> temporange 0.16             # 推子满量程 ±16%（缺省 0.1）
hypermixx> keylock wide                # 变调引擎：on（默认）| off | wide
hypermixx> key 1                       # 变调半音（占位：只记录，暂不发声）
hypermixx> beatjump 16
hypermixx> fx list                    # 列出fx
hypermixx> fx set eq low -0.5         # -1~1
hypermixx> fx set filter value  0.5
hypermixx> master fx list

# stems：离线分离 4 条轨，然后逐条控制（分离在后台跑，期间照常 play/loop/jump/sync）
hypermixx> deck0 stem separate         # CPU：首次 ~87s（3:39 曲目）+ ~2GB 内存；之后命中缓存 0s
hypermixx> deck0 stem separate --gpu   # CUDA：13s（同曲目，RTX 4050）；需 --features cuda 构建
hypermixx> deck0 stem separate --overlap 0.0   # 0.25→0.0：少 24% 时间，窗口边缘权重更大
hypermixx> deck0 stem acapella         # 只要人声（instrumental | drums | bass | full）
hypermixx> deck0 vocals level -0.5     # 单条 stem 电平（-1 = 精确静音）
hypermixx> deck0 vocals mute           # solo 仍然优先
hypermixx> deck0 vocals fx add filter  # 只给人声加滤波（deck0 fx … 则是整 deck 共用）
hypermixx> deck0 stem status           # 逐 stem 电平/静音/独奏

# 拍同步：deck1 对 deck0
hypermixx> deck1 sync phase pid        # 先对 BPM，再用 PI 追相位（pll 收敛后归零）
hypermixx> deck1 sync tempolock        # 两边共享一个 tempo，任一边 fader 都带动对方
hypermixx> deck0 sync set-leader       # 指定 master（target 即 leader，无参数）
hypermixx> deck1 nudge 0.04 0.5        # 临时 +4% 挪相位，0.5s 后自己松开（tempo 不变）
hypermixx> deck1 sync unlock           # 解锁：清 lock/align/nudgerate，tempo 保留
hypermixx> quit
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

## stem 分离

`stem separate` 把当前曲目离线分离成 4 条 stem（drums / bass / other / vocals）并**热装**进 deck：
分离在后台跑，期间照常 play / loop / jump / sync，装上的那一刻是一次"换源 jump"，所以无缝、不丢循环。

**它是怎么算的**：HTDemucs 的 ONNX 导出把输入钉在 **343,980 采样 = 7.8 秒**（`mix [1,2,343980]`
→ `stems [1,4,2,343980]`），所以整轨被**切成 7.8 秒的窗口逐窗过模型、再用三角权重叠加粘回去**
（重叠相加按权重和归一化）。下面两个旋钮改的就是这个"切与粘"，但改的是**两个不同的轴**。

### `--overlap <0..=0.5>`（默认 0.25）—— 空间轴：相邻窗口的估计互相融合

`stride = (1 − overlap) × 7.8 s`。窗口边缘是模型**上下文最少**的地方，重叠让边界处的采样改由
邻居窗口的**中部**主导。按 stride 一个周期精确统计：

| overlap | stride | 被两个窗口融合 | 主要由单窗口决定 | 权重压在窗口边缘（>50% 来自两端 0.5 s 内） |
|---|---|---|---|---|
| 0.00 | 7.80 s | 0.0% | 100.0% | **12.8%** |
| 0.10 | 7.02 s | 8.9% | 89.1% | 3.1% |
| **0.25** | 5.85 s | **26.7%** | 67.3% | **0.0%** |
| 0.50 | 3.90 s | 80.0% | 2.0% | 0.0% |

即 `0.25` 把"边缘估计主导"的采样从 **12.8% 压到 0.0%**；代价是窗口数 38→29，实测
**82 s → 61 s（−24%）**。窗口长度本身不可调（被导出钉死）。

### `--shifts <0..=2>`（默认 1）—— 相位轴：同一段音乐在不同窗口对位下重跑再平均

模型输出取决于**这段音乐落在窗口里的什么位置**。`shifts` 把整轨相对窗口栅格错开再跑一遍取平均，
把这种位置依赖抹掉。实测（比较 shifts=1 与 2 的差值在**窗口边缘**与**窗口中部的分布**）：
比值 0.95 / 0.93 —— **它是整条时间轴均匀地降方差，不是只修边界**，和 `overlap` 互补。
`0` 与 `1` 完全等价（单趟）。代价线性：`1 → 2` 是 **+98% 时间**（82 → 164 s）、+360 MB。

### `--gpu`（默认关）—— 换执行后端

| | 每 7.8 s 窗口 | 全曲 3:39 | 显存 |
|---|---|---|---|
| CPU（默认） | 2055 ms | 87 s | 1.98 GB |
| CUDA（RTX 4050 6 GB） | 214 ms | **13 s** | 3.2 GB |

需要 `--features cuda` 构建 + 宿主 CUDA 13 / cuDNN 9。`--gpu` 是**要求而不是建议**：CUDA 接管不了
整张图时直接报错，而不是悄悄在 CPU 上跑（慢 9 倍却看起来成功，正是这次踩过的坑 —— 见
[`vendor/charon-audio/PATCH.md`](vendor/charon-audio/PATCH.md)）。CPU 与 CUDA 的 Σ 残差一致，
但两者存在不同的缓存条目里。

### 三个旋钮都进缓存 key

因为它们都改变产出的音频。结果按**内容哈希**缓存到 `~/.cache/hypermixx/stems/`（每曲约 300 MB，
`stem cache prune [--keep <n>]` 清理），重复跑同一条曲目是 0 s。

**关于"哪个更好"，诚实说明**：我试过的两个客观代理都测不出这两个旋钮的质量差异 —— Σ 残差基本不动
（−30.1 / −30.4 / −30.7 dB，它量的是"四条加起来还等于原曲吗"，不是分离质量），二阶差分（找边界
突变）也只在 overlap 0.0 上略高（1.63× vs 1.15×，被音乐本身淹没，说明低重叠的代价**不是咔哒**而是
更微妙的边界音色不一致）。所以默认值留在原处，`--overlap 0.0` 是明确的"−24% 时间换一点边界质量"，
`--shifts 2` 是"+98% 时间换一点整体质量"，两者都值得自己听一遍再定。

## 脚本

```bash
cargo build --release --workspace   # 两个脚本默认吃 target/release 的二进制

./scripts/phase_probe.sh            # beatjump 精度：相位差增量是否恒定
./scripts/sync_phase.sh             # 双 deck 先后起播 → sync phase pid → 报稳态相位差
./scripts/sync_phase.sh target/release/hypermixx-cli test.mp3 122 0.75 24 none
                                    # 最后一个 mode 换 pid|linear|instant|tempo|none
```

`sync_phase.sh` 实测（122 BPM，起播差 0.75 拍 ≈ −120ms）：

| mode | 稳态相位差 | |
|---|---|---|
| `pid` | 0.0050 拍 (2.4ms) | 指数收敛，带 PI 超调 |
| `linear 2.0` | 0.0009 拍 (0.5ms) | 定斜率单调 |
| `instant` | 0.0000 拍 | 一次换流直接落点 |
| `tempo` / `none` | 漂移 0.0000 | 只对速不追相位，相位差恒定 |

## todo

1. loop sync
2. midi control
3. network streamming
4. realtime stems（离线分离 + 逐 stem 播放已完成，含 per-stem fx 配置与 MIDI：
   `docs/stem-plan.md`、`docs/commands.md` §3.7；未做的是短窗在线分离与 per-stem 波形）
5. slip loop(循环退出落 virtual/slip 位置;当前退出=落旧流停止处无缝续播、自然越过 out)
6. 收敛超时检测(目前只有 PLL 输出 ±5% 限幅兑底,误差关不上时会一直以 5% 跑,不会自动清 align)
7. TUI 按键式 nudge(需放行 `KeyEventKind::Release`,目前只支持定时/命令式)

## 许可证

MIT
