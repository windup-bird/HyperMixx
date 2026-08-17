//! 滚动波形显示模式（settings 落地前经 master 条按钮临时切换）。
//!
//! - `rgb`：mixxx 风格 RGB 波形——每像素列按频段归一化混色竖条
//!   （全频段 → 白、单频段主导 → 纯色），√ 包络。用户评价"效果好"。
//! - `bands`：三色——低/中/高三条实心带对称叠加（红/绿/蓝）。
enum WaveDisplayMode { rgb, bands }
