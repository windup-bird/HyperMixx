当前 Rust 生态中的 Stem 分离技术，已经形成了一条清晰的“**模型选型 → 推理后端 → 库集成**”技术路线。核心信息汇总如下：

### 🎯 核心模型：HTDemucs (Hybrid Transformer Demucs)

这是当前所有主流 Rust 方案的基石，源自 Meta 的开源模型 Demucs v4。

*   **分离能力**：标准版支持 **4 轨分离**（vocals, drums, bass, other），部分版本支持 **6 轨**（额外分离 guitar, piano）。
*   **模型规格与性能**：
    *   **标准 4-stem 模型**：约 **84 MB**，速度与质量平衡最佳。
    *   **6-stem 模型**：约 **84 MB**，适合需要吉他、钢琴分离的场景。
    *   **微调版 (Fine-tuned)**：约 **333 MB**，质量最高，但速度较慢。
    *   **ONNX 单文件版**：约 **316 MB** (FP32) 或 **166 MB** (FP16)，启动速度比微调版快约 **30%**。

### 🛠️ Rust 推理库选型

目前有三个主要的纯 Rust 库，均不依赖 Python，你的选择取决于对**集成便利性、性能或跨平台**的侧重。

**1. `stem-splitter-core`：集成最省心**
*   **核心特点**：基于 **ONNX Runtime**，内置模型自动下载与管理，提供进度回调，API 设计友好。
*   **适用场景**：快速集成到现有架构，作为后台异步分离任务的首选。
*   **许可证**：MIT 或 Apache-2.0，商用友好。

**2. `charon-audio`：性能与功能最强**
*   **核心特点**：支持 **ONNX Runtime** 和 **HuggingFace Candle** 双后端，原生支持 **CUDA / TensorRT / Metal** 等硬件加速，性能极强。
*   **适用场景**：对分离速度有极致要求，且愿意接受稍复杂的配置。
*   **性能参考**：在 M1 MacBook 上，分离一首 3 分钟歌曲仅需 **2.1 秒**。
*   **许可证**：MIT。

**3. `demucs-rs`：面向未来**
*   **核心特点**：基于 **Burn** 深度学习框架纯 Rust 实现，核心 `demucs-core` 可编译到 **WebAssembly**，通过 WebGPU 在浏览器中运行。
*   **适用场景**：有 Web 端或跨平台原生 CLI 需求。
*   **许可证**：需在项目仓库中确认。

### ⚡️ 性能表现

*   **CPU 基准**：在 Apple M4 Pro 上，ONNX Runtime CPU 处理一个 7.8 秒片段约 **1.6 秒**，完整 3 分钟歌曲约 **22 秒**。
*   **GPU 加速**：使用 NVIDIA L4 GPU 时，处理同一片段可缩短至约 **0.4 秒**，完整歌曲约 **5 秒**。
*   **内存占用**：首次运行需下载模型（约 200MB），处理时建议预留 **4GB+ RAM**。

### 🔗 与你现有架构的集成方案

核心思路是**离线异步分离**，完全不阻塞音频主线程。

1.  **触发时机**：在 `Command::Load` 之后，由 `library` 层发起一个异步分离任务。
2.  **处理流程**：后台线程将 `PcmPool` 中的 PCM 数据送入模型，分离完成后，为每个 stem 生成新的 `PcmPool`，并更新 `TrackInfo` 中的 `stems` 字段。
3.  **进度反馈**：通过回调机制向 CLI/TUI 报告分离进度。
4.  **无缝对接**：分离出的 stems 直接融入你现有的 `Mixer` 和 `Channel` 架构。`Channel` 的 `source` 字段可以是一个包含多个 stem `PcmPool` 的结构，播放时从对应 stem 读取数据。

### ⚠️ 关键风险与注意事项

*   **ONNX 导出复杂度**：HTDemucs 的 STFT/ISTFT 操作在导出 ONNX 时存在已知阻碍，**强烈建议直接使用现成的 ONNX 模型**（如 Hugging Face 上的 `StemSplitio/htdemucs-onnx`），规避自行导出的风险。
*   **平台支持**：确保所选库在 Linux、Windows 和 Android 上都能正确编译。纯 Rust 库（如 `stem-splitter-core`）在跨平台构建上通常更省心。
*   **模型下载与缓存**：首次使用需自动下载模型文件，务必实现可靠的缓存机制（如 SHA-256 校验），避免重复下载。

### 💎 总结

**首选 `stem-splitter-core` 进行快速集成验证**，它提供了最平滑的集成路径。如果后续发现性能瓶颈，再考虑迁移到 `charon-audio` 或基于 `demucs-core` 进行深度定制。这条路能最大化利用你已有的架构，同时为未来的 Stem FX 和 LLM 编曲等功能打下坚实基础。
