//! Overview 样式测试（P9）：waveMode 切换像素变化、beatgrid 已移除
//! （原全高白列 → 现为 P13 未播浅蒙层）、播放头仍渲染。
//! 像素级断言用 RepaintBoundary.toImage；未绘制处为全透明 (0,0,0,0)；
//! durSec>0 时全区盖 P13 蒙层（已播黑 α0.38 / 未播白 α0.10）。

import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/deck_controller.dart';
import 'package:hypermixx/engine/engine_controller.dart';
import 'package:hypermixx/engine/wave_display_mode.dart';
import 'package:hypermixx/engine/wave_model.dart';
import 'package:hypermixx/widgets/overview_wave.dart';

/// 满刻度、各频段不同电平：low=255 / mid=170 / high=110；时长 329s。
void _loudWave(DeckController dc) {
  final bytes = Uint8List(125000 * 8);
  const pattern = [255, 255, 170, 170, 110, 110, 255, 255];
  for (var i = 0; i < 125000; i++) {
    bytes.setRange(i * 8, i * 8 + 8, pattern);
  }
  dc.wave.full = WaveData(bytes);
  dc.wave.durationFrames = 48000 * 329;
  dc.wave.sampleRate = 48000;
}

Future<Uint8List> _pumpCapture(WidgetTester tester, DeckController dc) async {
  await tester.pumpWidget(
    MaterialApp(
      home: Scaffold(
        backgroundColor: const Color(0xFF1A1E24),
        body: SizedBox(width: 400, height: 64, child: OverviewWave(deck: dc)),
      ),
    ),
  );
  await tester.pump();
  final boundary = find.descendant(
      of: find.byType(OverviewWave), matching: find.byType(RepaintBoundary));
  final b = tester.renderObject<RenderRepaintBoundary>(boundary.first);
  late Uint8List bytes;
  await tester.runAsync(() async {
    final img = await b.toImage(pixelRatio: 1.0);
    final data = await img.toByteData(format: ui.ImageByteFormat.rawRgba);
    bytes = data!.buffer.asUint8List();
  });
  return bytes;
}

List<int> _px(Uint8List bytes, int w, int x, int y) {
  final i = (y * w + x) * 4;
  return [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]];
}

void main() {
  setUp(() {
    EngineController.instance.waveMode.value = WaveDisplayMode.rgb;
  });
  tearDown(() {
    EngineController.instance.waveMode.value = WaveDisplayMode.rgb;
  });

  testWidgets('切换 waveMode：同一像素渲染不同', (tester) async {
    final dc = DeckController(0);
    _loudWave(dc);
    dc.playhead.value = 100;
    final rgb = await _pumpCapture(tester, dc);
    EngineController.instance.waveMode.value = WaveDisplayMode.bands;
    await tester.pump();
    final bands = await _pumpCapture(tester, dc);
    // y=55：rgb 混色柱内（hp≈29 → y 35..64）；bands 的 LOW 主带内（≈50..64）
    final pRgb = _px(rgb, 400, 200, 55);
    final pBands = _px(bands, 400, 200, 55);
    expect(pRgb, isNot(equals(pBands)), reason: 'rgb=$pRgb bands=$pBands');
    expect(pRgb[3] > 200, isTrue, reason: 'rgb 有内容');
    expect(pBands[3] > 200, isTrue, reason: 'bands 有内容');
  });

  testWidgets('beats 不再画网格（原白列处透明）', (tester) async {
    final dc = DeckController(0);
    // 安静波形：列高 ≈ 14px 贴底，中上部透明
    final bytes = Uint8List(125000 * 8);
    for (var i = 0; i < 125000; i++) {
      for (var f = 0; f < 8; f++) {
        bytes[i * 8 + f] = 20;
      }
    }
    dc.wave.full = WaveData(bytes);
    dc.wave.durationFrames = 48000 * 329;
    dc.wave.sampleRate = 48000;
    // 100s → x≈121.6：旧实现画全高白竖线
    dc.beats = [50.0, 100.0, 150.0, 200.0];
    final out = await _pumpCapture(tester, dc);
    // playhead=0 → P13 全区未播浅蒙层（白 α0.10 ≈ α25 中性色）——旧网格
    // 是全高不透明白竖线（α≈230），两者可辨
    for (final x in [60, 61, 121, 122, 182, 243]) {
      final p = _px(out, 400, x, 32);
      expect(p[3] >= 18 && p[3] <= 40 && (p[0] - p[1]).abs() < 3, isTrue,
          reason: 'x=$x 网格已移除：仅未播浅蒙层（$p）');
    }
  });

  testWidgets('播放头仍渲染（白色竖线）', (tester) async {
    final dc = DeckController(0);
    _loudWave(dc);
    dc.playhead.value = 100;
    final out = await _pumpCapture(tester, dc);
    // x = 100/329×400 − 1 ≈ 120.6 → 列 121 全高白线
    final p = _px(out, 400, 121, 32);
    expect(p[3] > 200, isTrue, reason: '播放头应不透明');
    expect(p[0] > 200 && p[1] > 200 && p[2] > 200, isTrue,
        reason: '播放头白色: $p');
  });
}
