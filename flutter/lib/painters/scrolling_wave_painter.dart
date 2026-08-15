//! 滚动波形 painter（D5，锯齿修复 + 显示模式统一）：
//! ① 连续窗口数学（f64 列坐标，不量化到 1px）
//! ② scatter-max 分数聚合：每列投影到像素区间取最大，无 1px 步进顿挫；
//!    colsPerPx<1 高放大时线性插值防块状
//! ③ 播放线固定居中（P13，Serato/Traktor 式）：winStart = ph − winSecTrack/2，
//!    首尾不钳制——曲头前留白；曲尾播到底后波形止于中线、右半留白，线不动
//! ④ beatgrid 竖线 2px（下拍 3px）α0.35/0.7，间距 <4px 跳过（moiré 守卫）
//! ⑤ loop 区域绿填充/边界、cue/hotcue 橙竖线（P11.3）
//!
//! **两种显示模式共享同一形状**（每像素列 √(all) 包络 + 孤立尖刺抑制），
//! 仅染色不同（EngineController.waveMode 切换，settings 落地前经 master 条按钮）：
//! - **rgb**：每列按 lo/mi/hi 归一化混色（全频段 → 白、单频段主导 → 纯色），
//!   复刻 Slint waveform_texture.rs。
//! - **3-bands**：每列柱内按 low:mid:high 比例切红/绿/蓝三片（中心红、中绿、外蓝，
//!   上下镜像），外轮廓与 rgb 完全相同。
//!
//! repaint 由 `repaint` listenable 驱动（60Hz waveTick + waveRev + zoom + mode），
//! painter 在 paint() 里直接读 DeckController 的当前值——widget 不重建。

import 'dart:math' as math;

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import '../engine/wave_display_mode.dart';
import '../engine/wave_model.dart';

/// 尖刺抑制：孤立单列尖刺（高于两侧较高者 _kSpikeRatio 倍，且差 > _kSpikeMinPx 像素）
/// 压到邻居高度——去掉"极细锯齿"导致的滚动闪烁，保整体轮廓 crisp。可调。
const double _kSpikeRatio = 1.5;
const double _kSpikeMinPx = 8.0;

/// 共享形状：每像素列的包络 amp（已尖刺抑制）+ 三带电平（供染色）。
class _Shape {
  _Shape(this.amp, this.lo, this.mi, this.hi);

  final List<double> amp;
  final List<double> lo;
  final List<double> mi;
  final List<double> hi;
}

/// 3-bands 切片颜色：低=红（中心）、中=绿、高=蓝（外沿）。
const List<Color> _kSliceColors = [
  Color(0xFFE53935),
  Color(0xFF43A047),
  Color(0xFF1E88E5),
];

/// P13 拍轴统一横向坐标：返回 (winStartSecs, winSecTrack)。
/// 网格可用（gridBpm>0 && effBpm>0）时窗口按乐拍定宽：
///   winBeats = winSec×effBpm/60（展示乐拍数，effBpm = grid×rate），
///   winSecTrack = winBeats×60/gridBpm = winSec×effBpm/gridBpm。
/// 同步两轨 effBpm 相等 → 窗口拍宽相等、滚动速度 w/winSec 与 rate 无关、
/// 同一乐拍落同一 x、grid 线跨轨对齐；rate=1 逐像素与旧秒轴相同。
/// 无网格回退秒轴（winSecTrack = winSec）。
/// 播放线固定居中（P13）：winStart = ph − winSecTrack/2，首尾不钳制——
/// 曲头前 winStart 可为负（透明留白）；曲尾播到底窗口越出曲长、
/// 越界列读 0 → 右半留白，指针始终在 w/2（删 P11.4a 的尾端停右缘）。
/// 播放中显示用 displayPlayhead 外推（DeckController）保证滚动连续。
(double, double) waveWindowFor(
  double ph,
  double winSec,
  double gridBpm,
  double effBpm,
) {
  final winSecTrack =
      (gridBpm > 0 && effBpm > 0) ? winSec * effBpm / gridBpm : winSec;
  final winStart = ph - winSecTrack / 2;
  return (winStart, winSecTrack);
}

class ScrollingWavePainter extends CustomPainter {
  ScrollingWavePainter(this.deck, this.zoom, this.mode)
    : super(
        repaint: Listenable.merge([
          deck.waveTick,
          deck.waveRev,
          zoom,
          mode,
          deck.loopActive,
          deck.loopIn,
          deck.loopOut,
          deck.cuePoint,
          ...deck.hotcues,
        ]),
      );

  final DeckController deck;
  final ValueNotifier<double> zoom;
  final ValueNotifier<WaveDisplayMode> mode;

  /// 窗口秒数（master zoom）。
  double get winSec => 60.0 / zoom.value;

  @override
  void paint(Canvas canvas, Size size) {
    final w = size.width;
    final h = size.height;
    final wave = deck.wave;
    final dur = deck.duration.value;
    if (dur <= 0 || wave.colsTotal == 0) {
      // 占位：未载曲（P19 删未载入文字，只留居中播放头——提示提示文案
      // 归 deckinfo 的"点击左侧载入"）
      final px = w / 2;
      canvas.drawRect(
        Rect.fromLTWH(px - 1, 0, 2, h),
        Paint()..color = Colors.white.withValues(alpha: 0.9),
      );
      return;
    }

    // P13：播放线固定居中（displayPlayhead 外推 → 滚动连续）；
    // 拍轴统一横向坐标，无网格回退秒轴。
    final ph = deck.displayPlayhead;
    final (winStart, winSecTrack) = waveWindowFor(
      ph,
      winSec,
      deck.gridBpm.value,
      deck.effBpm.value,
    );

    // 共享形状：逐像素列聚合 → amp/lo/mi/hi + 孤立尖刺抑制（两模式一致）
    final shape = _computeShape(w, h, winStart, winSecTrack);

    if (mode.value == WaveDisplayMode.rgb) {
      _paintRgb(canvas, w, h, shape);
    } else {
      _paintBands(canvas, w, h, shape);
    }

    // 共享 overlay：beatgrid + loop/cue 标记 + 播放头
    _paintOverlay(canvas, w, h, winStart, winSecTrack, ph);
  }

  /// 共享形状计算：每像素列 scatter-max → al/lo/mi/hi，
  /// amp = √(al/255)·halfH；lead 留白列 amp=0；随后孤立尖刺抑制。
  /// winSecTrack 为窗口音轨秒（拍轴时 = winSec×rate，见 waveWindowFor）。
  _Shape _computeShape(
    double w,
    double h,
    double winStart,
    double winSecTrack,
  ) {
    final wave = deck.wave;
    final sr = wave.sampleRate.toDouble();
    final fpc = wave.framesPerCol.toDouble();
    final x0col = winStart * sr / fpc;
    final colsPerPx = winSecTrack * sr / fpc / w;
    final W = w.toInt();
    final halfH = h / 2 - 3;
    final amp = List<double>.filled(W, 0);
    final lo = List<double>.filled(W, 0);
    final mi = List<double>.filled(W, 0);
    final hi = List<double>.filled(W, 0);
    final out = Uint8List(8);
    // 曲头前留白（复刻 Slint lead_px）：winStart<0 时该段不画，深色背景透出
    final leadPx = winStart < 0 ? (-winStart / winSecTrack * w) : 0.0;
    for (var x = 0; x < W; x++) {
      if (leadPx > 0 && x < leadPx) continue;
      out.fillRange(0, 8, 0);
      wave.aggregateRange(
        x0col + x * colsPerPx,
        x0col + (x + 1) * colsPerPx,
        out,
      );
      final al = math.max(out[F.allP], out[F.allN]);
      lo[x] = math.max(out[F.lowP], out[F.lowN]).toDouble();
      mi[x] = math.max(out[F.midP], out[F.midN]).toDouble();
      hi[x] = math.max(out[F.highP], out[F.highN]).toDouble();
      if (al == 0) continue;
      amp[x] = math.sqrt(al / 255.0) * halfH;
    }
    _dampSpikes(amp);
    return _Shape(amp, lo, mi, hi);
  }

  /// 孤立单列尖刺抑制：把高于两侧较高者 1.5 倍且差 > 8px 的单列尖刺压平，
  /// 消除滚动时的"极细锯齿"闪烁；整体轮廓保持不变。两侧全静音 → 独立音符不压。
  void _dampSpikes(List<double> amp) {
    final n = amp.length;
    for (var i = 1; i < n - 1; i++) {
      if (amp[i] <= 0) continue;
      final m = math.max(amp[i - 1], amp[i + 1]);
      if (m <= 0) continue;
      if (amp[i] > m * _kSpikeRatio && amp[i] - m > _kSpikeMinPx) {
        amp[i] = m;
      }
    }
  }

  /// RGB 染色（复刻 Slint waveform_texture.rs）：每像素列按频段归一化混色竖条
  /// （颜色 = 各带 / 主导带，全频段亮列近白、单频段纯色），共享 amp 包络。
  void _paintRgb(Canvas canvas, double w, double h, _Shape shape) {
    final W = w.toInt();
    final cy = h / 2;
    final paint = Paint()..isAntiAlias = false;
    for (var x = 0; x < W; x++) {
      final a = shape.amp[x];
      if (a <= 0) continue;
      final mx = math.max(shape.lo[x], math.max(shape.mi[x], shape.hi[x]));
      if (mx <= 0) continue;
      final r = (shape.lo[x] / mx * 255).round().clamp(0, 255);
      final g = (shape.mi[x] / mx * 255).round().clamp(0, 255);
      final b = (shape.hi[x] / mx * 255).round().clamp(0, 255);
      paint.color = Color(0xFF000000 | (r << 16) | (g << 8) | b);
      canvas.drawRect(Rect.fromLTWH(x.toDouble(), cy - a, 1, a * 2), paint);
    }
  }

  /// 3-bands 染色：每列柱 [cy-amp, cy+amp] 按 low:mid:high 比例切三段
  /// （中心红、中绿、外蓝，上下镜像）；外轮廓 = amp，与 rgb 完全相同。
  void _paintBands(Canvas canvas, double w, double h, _Shape shape) {
    final W = w.toInt();
    final cy = h / 2;
    final paints = [
      Paint()..color = _kSliceColors[0],
      Paint()..color = _kSliceColors[1],
      Paint()..color = _kSliceColors[2],
    ];
    for (var x = 0; x < W; x++) {
      final a = shape.amp[x];
      if (a <= 0) continue;
      final l = shape.lo[x];
      final m = shape.mi[x];
      final hh = shape.hi[x];
      final total = l + m + hh;
      if (total <= 0) continue;
      // 各切片高度（占比 × 柱半高）；0.5px 以下的薄片跳过绘制但仍累计偏移
      final fr = l / total * a;
      final fg = m / total * a;
      final fb = hh / total * a;
      final xd = x.toDouble();
      // 上半 [cy, cy+a]：红(中心) → 绿 → 蓝(外沿)
      var top = cy;
      if (fr > 0.5) canvas.drawRect(Rect.fromLTWH(xd, top, 1, fr), paints[0]);
      top += fr;
      if (fg > 0.5) canvas.drawRect(Rect.fromLTWH(xd, top, 1, fg), paints[1]);
      top += fg;
      if (fb > 0.5) canvas.drawRect(Rect.fromLTWH(xd, top, 1, fb), paints[2]);
      // 下半 [cy-a, cy]：红 → 绿 → 蓝 镜像
      top = cy;
      if (fr > 0.5) {
        canvas.drawRect(Rect.fromLTWH(xd, top - fr, 1, fr), paints[0]);
      }
      top -= fr;
      if (fg > 0.5) {
        canvas.drawRect(Rect.fromLTWH(xd, top - fg, 1, fg), paints[1]);
      }
      top -= fg;
      if (fb > 0.5) {
        canvas.drawRect(Rect.fromLTWH(xd, top - fb, 1, fb), paints[2]);
      }
    }
  }

  /// 共享 overlay：beatgrid 竖线 + loop/cue 标记 + 播放头。
  void _paintOverlay(
    Canvas canvas,
    double w,
    double h,
    double winStart,
    double winSecTrack,
    double ph,
  ) {
    // beatgrid 竖线（P11.4d：2px/下拍 3px；moiré 守卫：<4px 间距跳过）
    final beats = deck.beats;
    if (beats.isNotEmpty) {
      final first = _lowerBound(beats, winStart);
      final endSec = winStart + winSecTrack;
      double prevX = -1e9;
      for (var i = first; i < beats.length; i++) {
        final t = beats[i];
        if (t >= endSec) break;
        final x = (t - winStart) / winSecTrack * w;
        if (x < 0 || x >= w || x - prevX < 4) continue;
        prevX = x;
        final isDown =
            deck.downbeats.isNotEmpty && _binarySearch(deck.downbeats, t) >= 0;
        canvas.drawRect(
          Rect.fromLTWH(x, 0, isDown ? 3 : 2, h),
          Paint()..color = Colors.white.withValues(alpha: isDown ? 0.7 : 0.35),
        );
      }
    }

    // P11.3 loop 区域：绿填充 + 边界（deck_pads 同系配色）；
    // x 越界由外层 ClipRect 裁剪。
    final li = deck.loopIn.value;
    final lo = deck.loopOut.value;
    if (deck.loopActive.value && lo > li) {
      final x0 = (li - winStart) / winSecTrack * w;
      final x1 = (lo - winStart) / winSecTrack * w;
      if (x1 > 0 && x0 < w) {
        canvas.drawRect(
          Rect.fromLTRB(math.max(0.0, x0), 0, math.min(w, x1), h),
          Paint()..color = const Color(0xFF2E7D32).withValues(alpha: 0.12),
        );
        final edge = Paint()
          ..color = const Color(0xFF66BB6A).withValues(alpha: 0.9);
        canvas.drawRect(Rect.fromLTWH(x0, 0, 2, h), edge);
        canvas.drawRect(Rect.fromLTWH(x1 - 2, 0, 2, h), edge);
      }
    }

    // P11.3 cue / hotcue：2px 竖线 + 顶部横帽（CUE 橙 / hotcue 深橙；
    // 空槽不画）。画在播放头之前 → 不遮播放头。
    void marker(double sec, Color color) {
      if (sec <= 0) return;
      final x = (sec - winStart) / winSecTrack * w;
      if (x < 0 || x >= w) return;
      final paint = Paint()..color = color.withValues(alpha: 0.9);
      canvas.drawRect(Rect.fromLTWH(x - 1, 0, 2, h), paint);
      canvas.drawRect(Rect.fromLTWH(x - 4, 0, 8, 6), paint);
    }

    final cue = deck.cuePoint.value;
    if (cue != null) marker(cue, const Color(0xFFFF7043));
    for (final hc in deck.hotcues) {
      final t = hc.value;
      if (t != null) marker(t, const Color(0xFFE65100));
    }

    // 播放线固定居中（P13：ph 居中窗口 → px 恒 = w/2；
    // 曲头/曲尾留白由 winStart 负值/越界列读 0 自然形成）
    final px = (ph - winStart) / winSecTrack * w;
    canvas.drawRect(
      Rect.fromLTWH(px - 1, 0, 2, h),
      Paint()..color = Colors.white.withValues(alpha: 0.9),
    );
  }

  @override
  bool shouldRepaint(ScrollingWavePainter old) =>
      old.deck != deck || old.zoom != zoom || old.mode != mode;
}

int _lowerBound(List<double> a, double v) {
  var lo = 0;
  var hi = a.length;
  while (lo < hi) {
    final mid = (lo + hi) >> 1;
    if (a[mid] < v) {
      lo = mid + 1;
    } else {
      hi = mid;
    }
  }
  return lo;
}

int _binarySearch(List<double> a, double v) {
  var lo = 0;
  var hi = a.length - 1;
  while (lo <= hi) {
    final mid = (lo + hi) >> 1;
    final m = a[mid];
    if ((m - v).abs() < 1e-6) return mid;
    if (m < v) {
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return -1;
}
