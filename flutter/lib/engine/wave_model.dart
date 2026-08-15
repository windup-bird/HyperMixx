//! 波形数据模型：分析事件 → 打包字节列（Dart 侧渲染用）。
//!
//! Rust 侧每列 8 字节（low_p/low_n/mid_p/mid_n/high_p/high_n/all_p/all_n），
//! wire 过来是 List<WireColumn>（每列一个堆对象，整曲 12 万列 ≈ 10MB+），
//! 这里压成 stride-8 的 Uint8List（整曲 ~1MB），painter 走字节索引。

import 'dart:typed_data';

import '../src/rust/api.dart';

/// 与 Rust SEG_COLS 一致：每段 6000 列（16s @48kHz/128 帧）。
const int kSegCols = 6000;

/// 字段偏移：0=low_p 1=low_n 2=mid_p 3=mid_n 4=high_p 5=high_n 6=all_p 7=all_n。
class F {
  static const int lowP = 0;
  static const int lowN = 1;
  static const int midP = 2;
  static const int midN = 3;
  static const int highP = 4;
  static const int highN = 5;
  static const int allP = 6;
  static const int allN = 7;
}

/// 一段打包波形列（packed.length = cols × 8）。
class WaveData {
  WaveData(this.packed) : cols = packed.length ~/ 8;

  final Uint8List packed;
  final int cols;

  int v(int i, int f) => packed[i * 8 + f];

  /// 对 [c0, c1) 列区间取 8 字段最大值累积到 out[8]（越界自动裁剪）。
  void maxOver(int c0, int c1, Uint8List out) {
    if (c0 < 0) c0 = 0;
    if (c1 > cols) c1 = cols;
    for (var i = c0; i < c1; i++) {
      final o = i * 8;
      for (var f = 0; f < 8; f++) {
        final v = packed[o + f];
        if (v > out[f]) out[f] = v;
      }
    }
  }
}

/// 渐进波形状态：Partial（每段稀疏，未分析段空缺）→ Full（Done 整体替换）。
class WaveModel {
  /// 已知段数上界（Segment 事件带 seg 索引）。
  int segCount = 0;
  final Map<int, WaveData> segs = {};
  WaveData? full;
  WaveData? fullOverview;
  int framesPerCol = 128;
  int sampleRate = 48000;
  int durationFrames = 0;

  double get durationSec => sampleRate <= 0 ? 0 : durationFrames / sampleRate;

  int get colsTotal => full?.cols ?? segCount * kSegCols;

  bool get isEmpty => full == null && segs.isEmpty;

  /// 读第 i 列的字段 f；无数据（未分析段/越界）返回 0。
  int colField(int i, int f) {
    if (i < 0 || i >= colsTotal) return 0;
    final d = full;
    if (d != null) return i < d.cols ? d.v(i, f) : 0;
    final sd = segs[i ~/ kSegCols];
    if (sd == null) return 0;
    final j = i - (i ~/ kSegCols) * kSegCols;
    return j < sd.cols ? sd.v(j, f) : 0;
  }

  /// 对 [c0, c1)（分数列界）取 8 字段最大值到 out[8]。
  /// colsPerPx >= 1 时逐整数列扫描；< 1 时线性插值相邻两列（高放大防块状）。
  void aggregateRange(double c0, double c1, Uint8List out) {
    final colsPerPx = c1 - c0;
    if (colsPerPx >= 1.0) {
      final i0 = c0.floor().clamp(0, colsTotal);
      final i1 = c1.floor().clamp(0, colsTotal);
      final d = full;
      if (d != null) {
        d.maxOver(i0, i1, out);
        return;
      }
      var seg = i0 ~/ kSegCols;
      final lastSeg = i1 ~/ kSegCols;
      while (seg <= lastSeg) {
        final sd = segs[seg];
        if (sd != null) {
          final lo = (i0 - seg * kSegCols).clamp(0, sd.cols);
          final hi = (i1 - seg * kSegCols).clamp(0, sd.cols);
          sd.maxOver(lo, hi, out);
        }
        seg++;
      }
    } else {
      // 每字段：列 c0、c0+1 线性插值
      final i0 = c0.floor();
      final t = c0 - i0;
      for (var f = 0; f < 8; f++) {
        final a = colField(i0, f);
        final b = colField(i0 + 1, f);
        out[f] = (a + (b - a) * t).round().clamp(0, 255);
      }
    }
  }

  void reset() {
    segCount = 0;
    segs.clear();
    full = null;
    fullOverview = null;
    framesPerCol = 128;
    sampleRate = 48000;
    durationFrames = 0;
  }
}

/// wire 列列表 → 打包 WaveData（Segment/Done 事件用）。
WaveData packCols(List<WireColumn> cols) {
  final p = Uint8List(cols.length * 8);
  var o = 0;
  for (final c in cols) {
    p[o++] = c.lowP;
    p[o++] = c.lowN;
    p[o++] = c.midP;
    p[o++] = c.midN;
    p[o++] = c.highP;
    p[o++] = c.highN;
    p[o++] = c.allP;
    p[o++] = c.allN;
  }
  return WaveData(p);
}
