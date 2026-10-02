# Stems：实现记录

这份文档记的是**已经建成什么、依据是什么**。相关的其它文档：

- 旋钮的完整语义与命令族：[`commands.md`](commands.md) §3.7、README「stem 分离」一节；
- 模型来源、许可、下载与校验：[`stem-info.md`](stem-info.md)；
- CUDA 补丁的全文说明与可重放 diff：[`../vendor/charon-audio/PATCH.md`](../vendor/charon-audio/PATCH.md)。

## 1. 现在能做什么

- **离线分离 + 热装**：`stem separate` 把当前曲目分离成 4 条 stem（drums/bass/other/vocals）。
  分离在后台跑，期间照常 play/loop/jump/sync；装上的那一刻是一次**换源 jump**，所以无缝、不丢循环。
- **逐 stem 播放**：每 stem 一条独立效果链（`flow_fx` 是模板，`[channel.stem_fx]` 是逐 stem 覆盖），
  加上逐 stem 的**电平 / 静音 / 独奏**——三件独立事实，由一处合成，所以 solo 压过 mute、取消静音能
  回到原电平。
- **MIDI**：`fader.stem.<stem>`、`stem.<stem>.mute|solo`（latch 按钮，引擎自己翻转状态）。
- **缓存**：结果按**内容哈希**落在 `~/.cache/hypermixx/stems/`（每曲约 300 MB），重复跑同一条
  曲目 0 s；`stem cache prune [--keep n]` / `clear`；跑着的任务可以 `stem cancel`。
- **模型获取**：按 SHA-256 校验、边下边算哈希、失败即删除；`huggingface.co` 不通时走 `hf-mirror.com`
  （`HYPERMIXX_HF_ENDPOINT` 可覆盖）。
- 代码在 `crates/hypermixx-stems`：`onnx`（默认开，可关：关掉后是 trait + mock + 缓存，不拉 ONNX
  Runtime）、`cuda`（可选，见 §4）。

## 2. 钉住的契约

```
in  mix   :: Float32 [1, 2, 343980]      ← 7.8 s @ 44.1 kHz，导出钉死，不可调
out stems :: Float32 [1, 4, 2, 343980]
```

- 整轨**切成 7.8 s 窗口逐窗过模型**，再用三角权重叠加粘回去（**按权重和归一化**，所以零重叠不会在
  边界塌陷）。重叠相加的窗口布局见 README 的统计表。
- **帧长精确**：Σ四条 stem 的帧数 == 输入帧数（测试钉住），所以换源不需要任何重采样或对齐。
- **源顺序按名字映射**：`ModelConfig::default()` 的顺序（drums,bass,vocals,other）与
  `htdemucs()`（drums,bass,other,vocals）**不同**——这是个地雷，`build_set` 已按名字映射，加新模型
  时不能改成按下标。
- 模型 pin：`StemSplitio/htdemucs-onnx/resolve/main/htdemucs.onnx`，**316 446 953 B**，
  sha256 `68d0bf16…cc5e74`，MIT，44.1 kHz，4 sources（`crates/hypermixx-stems/src/model.rs`）。

## 3. 两个旋钮（都改变产出的音频 → 都进缓存 key）

| 旋钮 | 默认 | 实测（`stem-test.mp3` 3:39） | 轴 |
|---|---|---|---|
| `--overlap <0..=0.5>` | 0.25 | `0.0` = **−24% 时间**（82→61 s）；代价：窗口边缘估计主导的采样从 0.0% 升到 12.8% | 空间：相邻窗口的估计互相融合 |
| `--shifts <0..=2>` | 1 | `2` = **+98% 时间**（82→164 s）、+360 MB；`0` 与 `1` 完全等价 | 相位：同一段音乐在不同窗口对位下重跑取平均 |
| `--gpu` / `--provider` | cpu | 见 §4 | 执行后端 |

**诚实说明**：我试过的两个客观代理都测不出这两个旋钮的**质量**差异——Σ 残差基本不动
（−30.1/−30.4/−30.7 dB，它量的是"四条加起来还等于原曲吗"），二阶差分（找边界突变）也只在
`overlap 0.0` 上略高（1.63× vs 1.15×，被音乐淹没，说明低重叠的代价**不是咔哒**而是更微妙的边界音色
不一致）。所以默认值留在原处，调它们值得自己听一遍。

## 4. GPU：为什么必须 fork（`vendor/charon-audio`）

**上游 charon-audio 0.1.2 根本没有"请求 CUDA"这一档**：`ExecutionProvider` 只有
`Cpu`/`CoreMl`/`Auto`。我最初以为"这个导出把 STFT 放在图内、只能跑 CPU"——**那个结论是错的**，
错了两层：

1. 缺的就是一个 enum 变体，不是模型或硬件不行。
2. 真凶是 `OnnxOptions::low_memory()`（`ModelConfig::htdemucs()` 继承它）里的
   **`disabled_optimizers: ["ConstantFolding"]`**：**一关掉 ConstantFolding，ORT 的 CUDA EP
   一个节点都不接**，整张 **1201 节点**的图全部落回 CPU EP。上游踩到了这个，把它归因成"导出只能
   在 CPU 上跑"。（我自己的第一个探针还因为 `default-features = false` 顺手关掉了 ort 的
   `copy-dylibs`/`tracing`，provider 库没拷到二进制旁、注册失败又被静默吞掉——"建了会话但一样慢"
   和"跑了但没收益"看起来完全一样。）

**补丁（`vendor/charon-audio`，236 K，`[patch.crates-io]` 指向它）**：

| 改动 | 作用 |
|---|---|
| `cuda` feature → `ort/cuda` | 镜像他们已有的 `coreml` 写法 |
| `ExecutionProvider::Cuda` + `build_session` 按 EP 分支 | CPU 路径一字未动 |
| Cuda 路径**保留 ConstantFolding**（过滤掉 `low_memory()` 的禁用项） | 这是"节点全落 CPU"的解药；只在 CUDA 生效 |
| Cuda 路径设 `session.disable_cpu_ep_fallback = 1` | 见下：不静默降级 |

`charon-audio-hypermixx.patch` 共 618 行，其中**功能性改动约 40 行**，其余是剥掉上游的
`bin`/`example`/`bench`/`test` 目标（上游把 1.6 MB 测试音频也打进包里）。

**实测（RTX 4050 Laptop 6 GB / CUDA 13.4 / cuDNN 9.26 / ORT 1.28 cuda13）**：

| | 每 7.8 s 窗口 | 全曲 3:39 | 显存 |
|---|---|---|---|
| CPU（**默认**） | 2055 ms | **87 s** | 1.98 GB |
| CUDA | 214 ms | **13 s** | 3.2 GB |

**9.6×**（端到端 6.7×）；开 `disable_cpu_ep_fallback` 后会话仍能建、能跑 ⇒ **1201 节点全在 CUDA**。

## 5. 不静默降级（这次踩坑换来的硬规则）

CUDA 不能接管整张图时，ORT 只是**悄悄**回到 CPU：慢 9 倍，看起来却完全成功。所以：

- 会话设 `disable_cpu_ep_fallback = 1`，任何节点落不到 CUDA 就**报错**；
- 建完会话再核对 `separator.provider()` 是否等于请求的后端，不一致也报错（双保险）；
- 失败信息翻译成可操作的话（需要 CUDA 13 / cuDNN 9 / `--features cuda` / provider 库在二进制旁，
  或去掉 `--gpu` 走 CPU）；
- 需要判断"是这个构建没有"还是"这个宿主没有"：`provider_available(cuda)` 看编译期，实际能否建
  会话看运行期。

**CPU 保持完整**：`cuda` 是可选 feature，默认构建不含任何 CUDA 依赖；`low_memory()` 的过滤只在
CUDA 路径生效，CPU 行为与加 GPU 之前逐位一致。

## 6. 缓存 key 与实测回归

- `separator.id()` = `charon-htdemucs-s{shifts}-o{overlap}-{provider}`，其中 shifts `0` 归一化成
  `1`（同一份 300 MB 不会存两份）。key 里带 provider，所以 CPU 与 CUDA 是两条独立缓存条目。
- 单测 24 个（`cargo test -p hypermixx-stems`），覆盖模型校验、缓存往返、选项策略与
  "**id 携带每一个会改变音频的选项**"。
- `crates/hypermixx-stems/tests/real_model.rs`（`#[ignore]`，要 302 MB 模型，opt-in 跑）把
  P0 的测量固化成回归：

```bash
cargo test --release -p hypermixx-stems --features cuda -- --ignored real_model --nocapture
# CPU  ≈ 17 s / CUDA ≈ 5.2–5.7 s（同一 25 s 片段），两边 Σstems − input ≈ −32.9 dB，
# 四条 stem 帧长精确 == 输入，provider 与请求一致
```

- P0 spike（`charon-audio` + `ort` 0.2.0-rc.13，100 s 素材）：分离 88 s、RTF 0.360、
  RSS 1.9 GB、4 stem 帧长精确（9 656 340 == 输入）、Σ−input −30.1 dB。

## 7. 有意没做的

- **短窗在线分离**（实时分离）与 **per-stem 波形**；
- **residual stem**：不做（Σ 残差 −30 dB 量级，听不出，代价是第五条流）；
- **6-stem**：引擎上限是 8 声道（4 stem × 2），`guitar/piano` 需要动引擎；
- **stem 源 mmap**：能省那约 600 MB 常驻，但要在 media 层做内存映射；
- **其它模型**：`htdemucs_fp16weights.onnx`（166 MB，下载减半）、单源专家（karaoke，算力约 1/4）
  ——`ModelSpec` 表已经预留，加模型 = 加一条数据 + `--model` 变真；
- **上游 PR**：这份补丁是按上游能接受的样子写的（`build_session` 按 EP 分支、不重复会话构造），
  提交给 `charon-audio` 是下一步。
