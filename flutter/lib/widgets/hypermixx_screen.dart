//! HyperMixx 主屏（P9 三列布局）：顶部错误条 + master 条 + 两行滚动波形 +
//! 下方三列 = 左 DeckPanel | MixerPanel（中心混音台）| 右 DeckPanel。
//! 每个 deck 列独立 RepaintBoundary（WaveStrip 内部再包一层），
//! 60Hz tick 只重绘两条滚动波形。

import 'package:flutter/material.dart';

import '../engine/engine_controller.dart';
import '../engine/wave_display_mode.dart';
import 'deck_panel.dart';
import 'mixer_panel.dart';
import 'wave_strip.dart';

class HyperMixxScreen extends StatefulWidget {
  const HyperMixxScreen({super.key});

  @override
  State<HyperMixxScreen> createState() => _HyperMixxScreenState();
}

class _HyperMixxScreenState extends State<HyperMixxScreen> {
  final EngineController _engine = EngineController.instance;

  @override
  void initState() {
    super.initState();
    _engine.start();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: const Color(0xFF1A1E24),
      body: Column(
        children: [
          _errorBanner(),
          // master 条置顶（zoom/音量/VU/波形模式）
          _masterBar(),
          // 两行波形（复刻 Slint main.slint）：deck1 上、deck2 下，全宽
          SizedBox(height: 190, child: WaveStrip(deck: _engine.decks[0])),
          SizedBox(height: 190, child: WaveStrip(deck: _engine.decks[1])),
          // 下方三列：左 deck 面板 | 中心混音台 | 右 deck 面板
          Expanded(
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Expanded(child: DeckPanel(deck: _engine.decks[0])),
                const VerticalDivider(width: 1, color: Color(0xFF2E353D)),
                const MixerPanel(),
                const VerticalDivider(width: 1, color: Color(0xFF2E353D)),
                Expanded(child: DeckPanel(deck: _engine.decks[1])),
              ],
            ),
          ),
        ],
      ),
    );
  }

  Widget _errorBanner() {
    return ValueListenableBuilder<String?>(
      valueListenable: _engine.error,
      builder: (context, err, _) {
        if (err == null) return const SizedBox.shrink();
        return Container(
          width: double.infinity,
          color: const Color(0xFFB71C1C),
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
          child: Text(
            err,
            style: const TextStyle(color: Colors.white, fontSize: 12),
          ),
        );
      },
    );
  }

  Widget _masterBar() {
    return Container(
      height: 40,
      color: const Color(0xFF23282F),
      padding: const EdgeInsets.symmetric(horizontal: 12),
      child: Row(
        children: [
          const Text(
            'ZOOM',
            style: TextStyle(color: Colors.white38, fontSize: 10),
          ),
          SizedBox(
            width: 160,
            child: ValueListenableBuilder<double>(
              valueListenable: _engine.zoom,
              builder: (_, z, _) => Slider(
                value: z,
                min: 1,
                max: 16,
                onChanged: (v) => _engine.zoom.value = v,
              ),
            ),
          ),
          const SizedBox(width: 16),
          const Icon(Icons.volume_up, size: 16, color: Colors.white38),
          SizedBox(
            width: 140,
            child: ValueListenableBuilder<double>(
              valueListenable: _engine.masterVolume,
              builder: (_, v, _) => Slider(
                value: v.clamp(0.0, 1.0),
                onChanged: (nv) => _engine.setMasterVolume(nv),
              ),
            ),
          ),
          const Spacer(),
          // 波形显示模式切换（settings 落地前的临时入口：RGB / 三色）
          ValueListenableBuilder<WaveDisplayMode>(
            valueListenable: _engine.waveMode,
            builder: (_, m, _) => TextButton(
              onPressed: _engine.toggleWaveMode,
              style: TextButton.styleFrom(
                padding: const EdgeInsets.symmetric(horizontal: 10),
                minimumSize: const Size(0, 26),
                backgroundColor: const Color(0xFF2E353D),
                foregroundColor: Colors.white70,
              ),
              child: Text(
                m == WaveDisplayMode.rgb ? 'RGB' : '3-bands',
                style: const TextStyle(fontSize: 11),
              ),
            ),
          ),
          const SizedBox(width: 16),
          // master VU
          SizedBox(
            width: 120,
            child: ValueListenableBuilder<double>(
              valueListenable: _engine.masterVu,
              builder: (_, vu, _) {
                return Row(
                  children: [
                    Expanded(
                      child: ClipRRect(
                        borderRadius: BorderRadius.circular(3),
                        child: LinearProgressIndicator(
                          value: vu.clamp(0.0, 1.0),
                          minHeight: 6,
                          backgroundColor: const Color(0xFF2E353D),
                          valueColor: const AlwaysStoppedAnimation(
                            Color(0xFF43A047),
                          ),
                        ),
                      ),
                    ),
                    const SizedBox(width: 6),
                    const Text(
                      'MASTER',
                      style: TextStyle(color: Colors.white38, fontSize: 10),
                    ),
                  ],
                );
              },
            ),
          ),
        ],
      ),
    );
  }
}
