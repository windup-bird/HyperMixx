# Hypermixx

基于Rust的跨平台混音软件，目前仅支持cli/tui。

## 架构

五层单向依赖，下层不感知上层：

```
cli ─┬─► audio ───┐
     ├─► library ─┼─► media ──► core
     └─► core ────┘
```

| crate | 职责 | 依赖 |
|---|---|---|
| `hypermixx-core` | 协议与类型：`Command`/`CommandResponse`、`DeckState`、`BeatGrid`、`Key`、`Source` | serde |
| `hypermixx-media` | 解码与内存池：`decode_file`（symphonia → 44.1k 立体声）、`PcmPool` | core |
| `hypermixx-audio` | 实时引擎：producer 线程 + cpal 输出、mixer/通道/FX 链、时间拉伸 | core, media |
| `hypermixx-library` | 离线分析：beat 网格编译、stratum-dsp 适配、波形峰值 | core, media |
| `hypermixx-cli` | 前端：行 REPL、`--tui` 终端界面、命令解析与补全 | 全部 |

外部依赖：`stratum-dsp`、`timestretch`。


## 安装

仅在Linux完成测试。

前置：Rust（2021 edition，1.70+）、音频输出设备；Linux 还需 ALSA 开发库。

```bash
sudo apt install libasound2-dev   # Debian/Ubuntu

git clone <repo> && cd HyperMixx
cargo build --workspace           # 或 --release
```

## 示例操作

```bash
cargo run -p hypermixx-cli - --tui
```

```text
hypermixx> load test.mp3 122
deck0 loaded: 18462369 frames (7:00.648)

hypermixx> play
hypermixx> beatjump 16
hypermixx> fx list                    # 列出fx
hypermixx> fx set eq low -0.5         # -1~1
hypermixx> fx set filter value  0.5
hypermixx> master fx list
hypermixx> quit
```

启动参数：

```bash
cargo run -p hypermixx-cli -- --backend auto        # 分析后端 auto | stratum | timestretch
cargo run -p hypermixx-cli -- --config topo.toml    # 自定义拓扑
cargo run -p hypermixx-cli -- --print-config        # 打印参考 TOML
```

## todo

1. loop sync
2. midi control
3. network streamming
4. realtime stems
5. slip loop(循环退出落 virtual/slip 位置;当前退出=落旧流停止处无缝续播、自然越过 out)

## 许可证

MIT
