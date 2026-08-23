//! 全区波形预览 painter（D6，平滑频段轮廓）：
//! 基线在底，三频段共享平滑采样和严格的内容边界。
//! 染色跟随 waveMode（模式切换由 painter 的 repaint 合并驱动）：
//! - rgb：单混色 (lo/mx, mi/mx, hi/mx)·255（复制 scrolling 归一化）；
//! - bands：低/中/高 按共享包络比例堆叠（红/绿/蓝）。
//! 全曲聚合（329s@~800px ≈ 150 列/px）；播放头 60Hz 自绘。
//! P13 已播蒙层：已播部分深色 ▓（黑 α0.38）、未播部分浅色 ░（白 α0.10），
//! 进度一眼可见；蒙层画在标记/播放头之下，不遮它们。

import 'dart:math' as math;

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import '../engine/wave_display_mode.dart';
import '../engine/wave_model.dart';
import 'wave_shape.dart';

class OverviewPainter extends CustomPainter {
  OverviewPainter(this.deck, this.mode)
    : super(
        repaint: Listenable.merge([
          deck.waveRev,
          deck.playhead,
          mode,
          // P11.3：停播时 playhead 不动，不加这些则设 loop/cue 后不重绘
          deck.loopActive,
          deck.loopIn,
          deck.loopOut,
          deck.cuePoint,
          ...deck.hotcues,
        ]),
      );

  final DeckController deck;
  final ValueListenable<WaveDisplayMode> mode;

  @override
  void paint(Canvas canvas, Size size) {
    if (size.width <= 0 || size.height <= 0) return;
    canvas.save();
    canvas.clipRect(Offset.zero & size);
    final w = size.width;
    final h = size.height;
    final wave = deck.wave;
    final colsTotal = wave.colsTotal;
    if (colsTotal == 0) {
      final tp = TextPainter(
        text: TextSpan(
          text: '分析中…',
          style: TextStyle(
            color: Colors.white.withValues(alpha: 0.25),
            fontSize: 12,
          ),
        ),
        textDirection: TextDirection.ltr,
      )..layout();
      tp.paint(canvas, Offset((w - tp.width) / 2, (h - tp.height) / 2));
      canvas.restore();
      return;
    }

    // 数据源：Done 后有 overview（4× 粗），否则用 detail/分段聚合
    final overview = wave.fullOverview ?? wave.full;
    final W = w.toInt();
    final maxH = math.max(0.0, h - 6);
    final rgb = mode.value == WaveDisplayMode.rgb;
    final samples = List.generate(3, (_) => List<double>.filled(W, 0));
    final normalized = List<double>.filled(W, 0);
    final out = Uint8List(9);
    for (var x = 0; x < W; x++) {
      if (overview != null) {
        // 整曲数据：列区间按比例映射
        final c0 = (x / W * overview.cols).floor();
        final c1 = ((x + 1) / W * overview.cols).floor();
        overview.maxOver(c0, c1, out);
      } else {
        // 渐进阶段：detail 列区间（分段稀疏）
        final c0 = (x / W * colsTotal);
        final c1 = ((x + 1) / W * colsTotal);
        wave.aggregateRange(c0, c1, out);
      }
      samples[0][x] = math.max(out[F.lowP], out[F.lowN]).toDouble();
      samples[1][x] = math.max(out[F.midP], out[F.midN]).toDouble();
      samples[2][x] = math.max(out[F.highP], out[F.highN]).toDouble();
      normalized[x] = overview?.hasNormalizedHeight == true
          ? out[F.normalizedHeight].toDouble()
          : math.sqrt(math.max(out[F.allP], out[F.allN]) / 255.0) * 255.0;
      out.fillRange(0, 8, 0);
    }
    final shape = buildWaveShape(
      low: samples[0],
      mid: samples[1],
      high: samples[2],
      maxHeight: maxH,
      normalizedHeight: normalized,
    );
    for (var x = 0; x < W; x++) {
      final values = [shape.low[x], shape.mid[x], shape.high[x]];
      if (rgb) {
        _paintRgbCol(canvas, values, shape.envelope, x, h);
      } else {
        _paintBandsCol(canvas, values, shape.envelope[x], x, h);
      }
    }

    // 播放头线（本 painter 自绘：Positioned overlay 在实机渲染异常）
    final durSec = wave.durationSec > 0
        ? wave.durationSec
        : deck.duration.value;
    if (durSec > 0) {
      // P13 已播蒙层：已播深 ▓ / 未播浅 ░（displayPlayhead 外推 → 播放中
      // 蒙层边界 60Hz 随动；画在 loop/cue/播放头之下，不遮标记）
      final maskX = clampDouble(deck.displayPlayhead / durSec * w, 0.0, w);
      canvas.drawRect(
        Rect.fromLTRB(0, 0, maskX, h),
        Paint()..color = const Color(0xFF000000).withValues(alpha: 0.38),
      );
      canvas.drawRect(
        Rect.fromLTRB(maskX, 0, w, h),
        Paint()..color = const Color(0xFFFFFFFF).withValues(alpha: 0.10),
      );

      // P11.3 loop 区域 + cue/hotcue 标记（按 durSec 比例；同滚动波形配色）
      final li = deck.loopIn.value;
      final lo = deck.loopOut.value;
      if (deck.loopActive.value && lo > li) {
        final x0 = li / durSec * w;
        final x1 = lo / durSec * w;
        canvas.drawRect(
          Rect.fromLTRB(math.max(0.0, x0), 0, math.min(w, x1), h),
          Paint()..color = const Color(0xFF2E7D32).withValues(alpha: 0.12),
        );
        final edge = Paint()
          ..color = const Color(0xFF66BB6A).withValues(alpha: 0.9);
        canvas.drawRect(Rect.fromLTWH(x0, 0, 1.5, h), edge);
        canvas.drawRect(Rect.fromLTWH(x1 - 1.5, 0, 1.5, h), edge);
      }

      void marker(double sec, Color color) {
        if (sec <= 0) return;
        final x = sec / durSec * w;
        if (x < 0 || x >= w) return;
        final paint = Paint()..color = color.withValues(alpha: 0.9);
        canvas.drawRect(Rect.fromLTWH(x, 0, 1.5, h), paint);
        canvas.drawRect(Rect.fromLTWH(x - 3, 0, 6, 4), paint);
      }

      final cue = deck.cuePoint.value;
      if (cue != null) marker(cue, const Color(0xFFFF7043));
      for (final hc in deck.hotcues) {
        final t = hc.value;
        if (t != null) marker(t, const Color(0xFFE65100));
      }

      final px = clampDouble(
        deck.displayPlayhead / durSec * w - 1,
        -2.0,
        w + 2.0,
      );
      canvas.drawRect(
        Rect.fromLTWH(px, 0, 2, h),
        Paint()..color = Colors.white.withValues(alpha: 0.9),
      );
    }
    canvas.restore();
  }

  /// RGB 连续轮廓：相邻采样点连接成填充路径，避免 1px 柱状跳变。
  void _paintRgbCol(
    Canvas canvas,
    List<double> values,
    List<double> envelope,
    int x,
    double h,
  ) {
    final lo = values[0];
    final mi = values[1];
    final hi = values[2];
    final mx = math.max(lo, math.max(mi, hi));
    if (mx <= 0) return;
    final height = envelope[x];
    final nextHeight = envelope[math.min(x + 1, envelope.length - 1)];
    final r = (lo / mx * 255).round().clamp(0, 255);
    final g = (mi / mx * 255).round().clamp(0, 255);
    final b = (hi / mx * 255).round().clamp(0, 255);
    final paint = Paint()..color = Color.fromARGB(220, r, g, b);
    if (height <= 0.5) return;
    final path = Path()
      ..moveTo(x.toDouble(), h - height)
      ..lineTo((x + 1).toDouble(), h - nextHeight)
      ..lineTo((x + 1).toDouble(), h)
      ..lineTo(x.toDouble(), h)
      ..close();
    canvas.drawPath(path, paint);
  }

  /// bands 三带按原始频段比例堆叠到 Rust 输出的共享包络内。
  void _paintBandsCol(
    Canvas canvas,
    List<double> values,
    double envelope,
    int x,
    double h,
  ) {
    const colors = [Color(0xFFE53935), Color(0xFF43A047), Color(0xFF1E88E5)];
    final total = values[0] + values[1] + values[2];
    if (total <= 0) return;
    var y = h;
    for (var b = 0; b < 3; b++) {
      final height = values[b] / total * envelope;
      if (height > 0.5) {
        canvas.drawRect(
          Rect.fromLTWH(x.toDouble(), y - height, 1, height),
          Paint()..color = colors[b].withValues(alpha: 0.9),
        );
      }
      y -= height;
    }
  }

  @override
  bool shouldRepaint(OverviewPainter old) =>
      old.deck != deck || old.mode.value != mode.value;
}
