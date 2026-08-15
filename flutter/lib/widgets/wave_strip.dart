//! 滚动波形条：CustomPaint + 点击/拖动 seek（100ms 节流）。
//! RepaintBoundary 隔离：60Hz 只重绘本 strip 的 painter。

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import '../engine/engine_controller.dart';
import '../painters/scrolling_wave_painter.dart';

class WaveStrip extends StatefulWidget {
  const WaveStrip({super.key, required this.deck});

  final DeckController deck;

  @override
  State<WaveStrip> createState() => _WaveStripState();
}

class _WaveStripState extends State<WaveStrip> {
  DateTime _lastSeek = DateTime.fromMillisecondsSinceEpoch(0);

  void _seekAt(double dx, double w) {
    final engine = EngineController.instance;
    final dc = widget.deck;
    final winSec = 60.0 / engine.zoom.value;
    final dur = dc.duration.value;
    if (dur <= 0) return;
    // 与 painter 同款窗口数学（P13 拍轴 waveWindowFor——点击映射必须与
    // 显示一致，同样用外推播放头；播放线固定居中，无 ended 分支）
    final (winStart, winSecTrack) = waveWindowFor(
      dc.displayPlayhead,
      winSec,
      dc.gridBpm.value,
      dc.effBpm.value,
    );
    final sec = clampDouble(winStart + dx / w * winSecTrack, 0.0, dur);
    // 拖动节流 100ms（seek 会重锚 reader + 重置 keylock）
    final now = DateTime.now();
    if (now.difference(_lastSeek).inMilliseconds < 100) return;
    _lastSeek = now;
    engine.seekTo(widget.deck.deck, sec);
  }

  @override
  Widget build(BuildContext context) {
    final engine = EngineController.instance;
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
              child: CustomPaint(
                size: Size(w, h),
                painter: ScrollingWavePainter(
                  widget.deck,
                  engine.zoom,
                  engine.waveMode,
                ),
              ),
            ),
          );
        },
      ),
    );
  }
}
