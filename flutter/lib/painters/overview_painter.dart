//! 全区波形预览 painter（D6，折叠整流半波）：
//! 基线在底，每像素列 p 全 α、n α0.55（负半周折上、层次可见）。
//! 染色跟随 waveMode（模式切换由 painter 的 repaint 合并驱动）：
//! - rgb：单混色 (lo/mx, mi/mx, hi/mx)·255（复制 scrolling 归一化）；
//! - bands：低/中/高 三带自底向上堆叠（红/绿/蓝）。
//! 全曲聚合（329s@~800px ≈ 150 列/px）；播放头 60Hz 自绘。
//! P13 已播蒙层：已播部分深色 ▓（黑 α0.38）、未播部分浅色 ░（白 α0.10），
//! 进度一眼可见；蒙层画在标记/播放头之下，不遮它们。

import 'dart:math' as math;

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import '../engine/wave_display_mode.dart';
import '../engine/wave_model.dart';

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
      return;
    }

    // 数据源：Done 后有 overview（4× 粗），否则用 detail/分段聚合
    final overview = wave.fullOverview ?? wave.full;
    final W = w.toInt();
    final maxH = h - 6;
    const bandAlpha = 0.9;
    const foldAlpha = 0.55 * 0.9;
    final out = Uint8List(8);
    final rgb = mode.value == WaveDisplayMode.rgb;
    final paints = [
      Paint()..color = const Color(0xFFE53935).withValues(alpha: bandAlpha),
      Paint()..color = const Color(0xFF43A047).withValues(alpha: bandAlpha),
      Paint()..color = const Color(0xFF1E88E5).withValues(alpha: bandAlpha),
    ];
    final paintsFolded = [
      Paint()..color = const Color(0xFFE53935).withValues(alpha: foldAlpha),
      Paint()..color = const Color(0xFF43A047).withValues(alpha: foldAlpha),
      Paint()..color = const Color(0xFF1E88E5).withValues(alpha: foldAlpha),
    ];
    final bandPs = [F.lowP, F.midP, F.highP];
    final bandNs = [F.lowN, F.midN, F.highN];

    for (var x = 0; x < W; x++) {
      out.fillRange(0, 8, 0);
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
      if (rgb) {
        _paintRgbCol(canvas, out, x, maxH, h);
      } else {
        _paintBandsCol(canvas, out, bandPs, bandNs, paints, paintsFolded,
            x, maxH, h);
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
  }

  /// rgb 单混色列：lo/mi/hi 取 max(p,n) 归一化混色，p 全 α、n α0.55
  /// 折叠堆叠（几何与 bands 一致：自底向上 p 在下、n 在上）。
  void _paintRgbCol(Canvas canvas, Uint8List out, int x, double maxH, double h) {
    final lo = math.max(out[F.lowP], out[F.lowN]).toDouble();
    final mi = math.max(out[F.midP], out[F.midN]).toDouble();
    final hi = math.max(out[F.highP], out[F.highN]).toDouble();
    final mx = math.max(lo, math.max(mi, hi));
    if (mx <= 0) return;
    final pTotal = (out[F.lowP] + out[F.midP] + out[F.highP]).toDouble();
    final nTotal = (out[F.lowN] + out[F.midN] + out[F.highN]).toDouble();
    final total = pTotal + nTotal;
    final k = (total > 255 ? 255.0 / total : 1.0) / 255.0;
    final hp = pTotal * k * maxH;
    final hn = nTotal * k * maxH;
    final r = (lo / mx * 255).round().clamp(0, 255);
    final g = (mi / mx * 255).round().clamp(0, 255);
    final b = (hi / mx * 255).round().clamp(0, 255);
    final paint = Paint()..color = Color(0xFF000000 | (r << 16) | (g << 8) | b);
    if (hp > 0.5) {
      canvas.drawRect(Rect.fromLTWH(x.toDouble(), h - hp, 1, hp), paint);
    }
    if (hn > 0.5) {
      final paintN = Paint()
        ..color = paint.color.withValues(alpha: 0.55);
      canvas.drawRect(
        Rect.fromLTWH(x.toDouble(), h - hp - hn, 1, hn),
        paintN,
      );
    }
  }

  /// bands 三带堆叠：单列 Σ(p+n) 可达 6×255，按列总高缩放到 ≤ maxH。
  void _paintBandsCol(
    Canvas canvas,
    Uint8List out,
    List<int> bandPs,
    List<int> bandNs,
    List<Paint> paints,
    List<Paint> paintsFolded,
    int x,
    double maxH,
    double h,
  ) {
    final total =
        out[F.lowP] +
        out[F.lowN] +
        out[F.midP] +
        out[F.midN] +
        out[F.highP] +
        out[F.highN];
    final k = (total > 255 ? 255.0 / total : 1.0) / 255.0;
    var y = h;
    for (var b = 0; b < 3; b++) {
      final p = out[bandPs[b]];
      final n = out[bandNs[b]];
      final hp = p * k * maxH;
      final hn = n * k * maxH;
      if (hp > 0.5) {
        canvas.drawRect(
          Rect.fromLTWH(x.toDouble(), y - hp, 1, hp),
          paints[b],
        );
      }
      if (hn > 0.5) {
        canvas.drawRect(
          Rect.fromLTWH(x.toDouble(), y - hp - hn, 1, hn),
          paintsFolded[b],
        );
      }
      y -= hp + hn;
    }
  }

  @override
  bool shouldRepaint(OverviewPainter old) =>
      old.deck != deck || old.mode.value != mode.value;
}
