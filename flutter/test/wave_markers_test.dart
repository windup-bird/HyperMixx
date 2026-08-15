// P11.3 loop/cue 波形提示像素级验证：滚动波形（WaveStrip）与预览
//（OverviewWave）的 loop 区域绿填充/边界、cue 橙竖线、hotcue 深橙竖线；
// 空槽不产生像素、标记不遮播放头。
// 复用 playhead_center_test / overview_style_test 的满刻度波形 + toImage 套路。

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
import 'package:hypermixx/widgets/wave_strip.dart';

/// 满刻度、各频段不同电平：low=255 / mid=170 / high=110（bands 模式下
/// 波形柱中段为纯红 (0xE53935)——标记混色后的 RGB 可解析判定）。
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

Future<Uint8List> _capture(WidgetTester tester, Finder boundaryOf) async {
  final boundary = tester.renderObject<RenderRepaintBoundary>(
    find.descendant(of: boundaryOf, matching: find.byType(RepaintBoundary)).first,
  );
  late Uint8List bytes;
  await tester.runAsync(() async {
    final img = await boundary.toImage(pixelRatio: 1.0);
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
    EngineController.instance.waveMode.value = WaveDisplayMode.bands;
  });
  tearDown(() {
    EngineController.instance.waveMode.value = WaveDisplayMode.bands;
  });

  Future<void> pumpStrip(WidgetTester tester, DeckController dc) async {
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          backgroundColor: const Color(0xFF1A1E24),
          body: SizedBox(width: 800, height: 190, child: WaveStrip(deck: dc)),
        ),
      ),
    );
    await tester.pump();
  }

  Future<void> pumpOverview(WidgetTester tester, DeckController dc) async {
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          backgroundColor: const Color(0xFF1A1E24),
          body: SizedBox(width: 400, height: 64, child: OverviewWave(deck: dc)),
        ),
      ),
    );
    await tester.pump();
  }

  testWidgets('滚动波形：loop 区域绿边界 + 绿填充，区间外无绿', (tester) async {
    final dc = DeckController(0);
    _loudWave(dc);
    dc.duration.value = 329.0;
    dc.playhead.value = 92.5; // winStart=62.5，指针 x=400
    dc.loopActive.value = true;
    dc.loopIn.value = 100.0;
    dc.loopOut.value = 160.0; // x0 = 37.5/60*800 = 500.0 整
    await pumpStrip(tester, dc);

    final bytes = await _capture(tester, find.byType(WaveStrip));
    final w = 800;
    // 边界 2px 亮绿（0xFF66BB6A α0.9 混在红色波形上，x=500 整像素全覆盖）
    final edge = _px(bytes, w, 500, 60);
    expect(edge[1] > edge[0] + 40 && edge[1] > edge[2] + 40, isTrue,
        reason: 'loop in 边界应亮绿（g 主导）: $edge');
    // 填充 0x2E7D32 α0.12：顶行（无波形柱）绿 tint 可辨
    final fill = _px(bytes, w, 600, 1);
    expect(fill[3] >= 20 && fill[1] > fill[0] + 5, isTrue,
        reason: 'loop 区域内应有绿填充（顶行）: $fill');
    // 区间外顶行无填充（透明）
    final outside = _px(bytes, w, 300, 1);
    expect(outside[3] < 12, isTrue, reason: 'loop 区间外顶行应透明: $outside');
    // 区间外波形保持纯红（无绿 tint）
    final red = _px(bytes, w, 300, 60);
    expect(red[0] > red[1] + 100, isTrue, reason: '区间外波形应保持红色: $red');
    // 播放头不被 loop 边界遮住（指针居中 x=400，白）
    final ph = _px(bytes, w, 400, 60);
    expect(ph[0] > 200 && ph[1] > 200 && ph[2] > 200, isTrue,
        reason: '播放头应保持白色: $ph');
  });

  testWidgets('滚动波形：cue 橙竖线 + 顶帽，不遮播放头', (tester) async {
    final dc = DeckController(0);
    _loudWave(dc);
    dc.duration.value = 329.0;
    dc.playhead.value = 92.5; // winStart=62.5，指针 x=400
    dc.cuePoint.value = 115.0; // x = 52.5/60*800 = 700.0 整
    await pumpStrip(tester, dc);

    final bytes = await _capture(tester, find.byType(WaveStrip));
    final w = 800;
    // 竖线（0xFFFF7043 α0.9 混在红色波形上）
    final line = _px(bytes, w, 700, 60);
    expect(line[0] > 220 && line[1] > 60 && line[1] < 160 && line[2] < 120,
        isTrue, reason: 'cue 竖线应橙色（r 高、b 低）: $line');
    // 顶帽（y=2 在 6px 帽内）
    final cap = _px(bytes, w, 700, 2);
    expect(cap[0] > 200 && cap[1] > 60 && cap[1] < 160, isTrue,
        reason: 'cue 顶帽应橙色: $cap');
    // 播放头不被 cue 遮住
    final ph = _px(bytes, w, 400, 60);
    expect(ph[0] > 200 && ph[1] > 200 && ph[2] > 200, isTrue,
        reason: '播放头应保持白色: $ph');
  });

  testWidgets('滚动波形：hotcue 深橙竖线，空槽不产生像素', (tester) async {
    final dc = DeckController(0);
    _loudWave(dc);
    dc.duration.value = 329.0;
    dc.playhead.value = 92.5;
    dc.hotcues[3].value = 85.0; // x = 22.5/60*800 = 300.0 整
    await pumpStrip(tester, dc);

    final bytes = await _capture(tester, find.byType(WaveStrip));
    final w = 800;
    // hotcue（0xFFE65100 α0.9 混在红色波形上）
    final line = _px(bytes, w, 300, 60);
    expect(line[0] > 200 && line[1] < 110 && line[2] < 20, isTrue,
        reason: 'hotcue 竖线应深橙（r 高、g/b 低）: $line');
    // 空槽（无标记处）顶行透明
    final empty = _px(bytes, w, 700, 1);
    expect(empty[3] < 12, isTrue, reason: '空槽处不应有像素: $empty');
  });

  testWidgets('overview：loop 区域绿边界/填充 + cue/hotcue 竖线', (tester) async {
    // 波形柱（含折叠负半）最高到 y≈6：y=4 无波形（仅 P13 蒙层薄底）→ 标记直出
    EngineController.instance.waveMode.value = WaveDisplayMode.rgb;
    final dc = DeckController(0);
    _loudWave(dc);
    dc.playhead.value = 0.0;
    dc.loopActive.value = true;
    dc.loopIn.value = 100.0;
    dc.loopOut.value = 160.0; // x0 = 100/329*400 ≈ 121.6，x1 ≈ 194.5
    dc.cuePoint.value = 246.75; // x = 300.0 整
    dc.hotcues[0].value = 164.5; // x = 200.0 整
    await pumpOverview(tester, dc);

    final bytes = await _capture(tester, find.byType(OverviewWave));
    const w = 400;
    final edge = _px(bytes, w, 122, 4);
    expect(edge[3] > 200 && edge[1] > edge[0] + 40 && edge[1] > edge[2] + 40,
        isTrue, reason: 'overview loop in 边界应亮绿: $edge');
    final fill = _px(bytes, w, 150, 4);
    expect(fill[3] >= 20 && fill[3] <= 60 && fill[1] > fill[0] + 5, isTrue,
        reason: 'overview loop 区域内应有绿填充: $fill');
    final outside = _px(bytes, w, 60, 4);
    // P13 已播蒙层：playhead=0 → 全区未播浅蒙层（白 α0.10 ≈ α25，非透明）
    expect(outside[3] >= 18 && outside[3] <= 40, isTrue,
        reason: 'overview 未播区应有浅蒙层（P13）: $outside');
    // cue 橙竖线（x=300 整 → 像素 300 全覆盖）
    final cue = _px(bytes, w, 300, 4);
    expect(cue[0] > 200 && cue[1] > 60 && cue[1] < 150 && cue[2] < 100, isTrue,
        reason: 'overview cue 应橙色: $cue');
    // hotcue 深橙（x=200 整）
    final hot = _px(bytes, w, 200, 4);
    expect(hot[0] > 200 && hot[1] < 100 && hot[2] < 15, isTrue,
        reason: 'overview hotcue 应深橙: $hot');
  });
}
