//! 全区半波预览：painter 自绘波形 + 播放头，
//! repaint 由 waveRev（数据）、playhead（60Hz）与 waveMode 合并驱动。

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import '../engine/engine_controller.dart';
import '../painters/overview_painter.dart';

class OverviewWave extends StatefulWidget {
  const OverviewWave({super.key, required this.deck});

  final DeckController deck;

  @override
  State<OverviewWave> createState() => _OverviewWaveState();
}

class _OverviewWaveState extends State<OverviewWave> {
  DateTime _lastSeek = DateTime.fromMillisecondsSinceEpoch(0);

  void _seekAt(double dx, double w) {
    final dc = widget.deck;
    final dur = dc.wave.durationSec > 0 ? dc.wave.durationSec : dc.duration.value;
    if (dur <= 0) return;
    final now = DateTime.now();
    if (now.difference(_lastSeek).inMilliseconds < 100) return;
    _lastSeek = now;
    EngineController.instance
        .seekTo(widget.deck.deck, clampDouble(dx / w * dur, 0.0, dur));
  }

  @override
  Widget build(BuildContext context) {
    final dc = widget.deck;
    return RepaintBoundary(
      child: LayoutBuilder(
        builder: (context, cons) {
          final w = cons.maxWidth;
          final h = cons.maxHeight;
          return GestureDetector(
            behavior: HitTestBehavior.opaque,
            onTapDown: (d) => _seekAt(d.localPosition.dx, w),
            onHorizontalDragStart: (d) => _seekAt(d.localPosition.dx, w),
            onHorizontalDragUpdate: (d) => _seekAt(d.localPosition.dx, w),
            child: ClipRect(
              // 播放头由 OverviewPainter 自绘（Positioned overlay 在实机渲染异常）
              child: CustomPaint(
                size: Size(w, h),
                painter: OverviewPainter(
                  dc,
                  EngineController.instance.waveMode,
                ),
              ),
            ),
          );
        },
      ),
    );
  }
}
