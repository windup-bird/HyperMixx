// 播放线固定居中（P13，Serato/Traktor 式）渲染验证：painter 输出像素级断言。
// winStart = ph - winSec/2，首尾不钳制：可为负 → 曲头前留白；
// 曲尾窗口越出曲长 → 越界列读 0、右半留白，线恒在 w/2。
//
// 用 RepaintBoundary.toImage 抓真实渲染像素：
// - 未播放 ph=0  → 指针在窗口中心，左侧留白（深色背景）
// - 播放中 ph=100 → 指针仍在中心，无留白
// - 尾端 ph=320  → 指针仍在中心，右段越界留白（无尾端钳制）
// - 播完 ph=329  → 指针恒居中，波形止于中线、右半留白（P13）

import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/deck_controller.dart';
import 'package:hypermixx/engine/wave_model.dart';
import 'package:hypermixx/widgets/wave_strip.dart';

/// 给 deck 的 wave 填入满刻度数据，各频段不同电平：
/// low=255 / mid=170 / high=110（RGB 模式下混色为非白橙色，`_bandColor` 可判定；
/// 全 255 会在 RGB 模式渲染成白色竖条、与白色播放头无法区分）。
/// 需覆盖整个 329s 窗口（48kHz/128 = 375 列/s → 329s ≈ 123375 列），
/// 否则窗口右段无数据（测试会断言"有波形内容"而失败）。
void _loudWave(DeckController dc) {
  final bytes = Uint8List(125000 * 8);
  const pattern = [255, 255, 170, 170, 110, 110, 255, 255];
  for (var i = 0; i < 125000; i++) {
    bytes.setRange(i * 8, i * 8 + 8, pattern);
  }
  dc.wave.full = WaveData(bytes);
}

Future<Uint8List> _capture(WidgetTester tester) async {
  final boundary = tester.renderObject<RenderRepaintBoundary>(find
      .descendant(
          of: find.byType(WaveStrip), matching: find.byType(RepaintBoundary))
      .first);
  late Uint8List bytes;
  await tester.runAsync(() async {
    final img = await boundary.toImage(pixelRatio: 1.0);
    final data = await img.toByteData(format: ui.ImageByteFormat.rawRgba);
    bytes = data!.buffer.asUint8List();
  });
  return bytes;
}

/// (x, y) 处 RGBA。
List<int> _px(Uint8List bytes, int w, int x, int y) {
  final i = (y * w + x) * 4;
  return [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]];
}

// toImage 只抓 RepaintBoundary 层：未绘制处为全透明 (0,0,0,0)，不是 Scaffold 背景色。
bool _nearTransparent(List<int> p) => p[3] < 12;

bool _nearWhite(List<int> p) => p[0] > 200 && p[1] > 200 && p[2] > 200;

/// 有波形带：不透明且非白色指针。
bool _bandColor(List<int> p) => p[3] > 200 && !_nearWhite(p);

void main() {
  Future<void> pump(WidgetTester tester, DeckController dc) async {
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

  testWidgets('playhead centered when not playing (ph=0, blank lead)', (tester) async {
    final dc = DeckController(0);
    _loudWave(dc);
    dc.duration.value = 329.0;
    dc.playhead.value = 0.0; // 未播放
    await pump(tester, dc);

    final bytes = await _capture(tester);
    final w = 800;
    // 中心指针：白色竖线在 x≈400
    expect(_nearWhite(_px(bytes, w, 400, 60)), isTrue,
        reason: 'ph=0 指针应居中（x=400）');
    // 左侧 1/10 处：曲头前留白（透明，未画带）
    expect(_nearTransparent(_px(bytes, w, 80, 60)), isTrue,
        reason: '曲头前应留白（winStart<0）');
    // 右侧 9/10 处：波形带已画
    expect(_bandColor(_px(bytes, w, 720, 60)), isTrue,
        reason: '窗口右侧应有波形内容');
  });

  testWidgets('playhead stays centered mid-track even when not playing', (tester) async {
    final dc = DeckController(0);
    _loudWave(dc);
    dc.duration.value = 329.0;
    dc.playhead.value = 100.0; // 暂停在中段
    await pump(tester, dc);

    final bytes = await _capture(tester);
    final w = 800;
    expect(_nearWhite(_px(bytes, w, 400, 60)), isTrue,
        reason: 'ph=100 暂停，指针仍应居中');
    // 无留白：winStart=70>0，最左侧已有波形
    expect(_bandColor(_px(bytes, w, 20, 60)), isTrue,
        reason: '中段暂停不应有留白');
  });

  testWidgets('empty deck: centered playhead（空条也画指针）', (tester) async {
    final dc = DeckController(0); // 无曲目：duration=0、wave 空 → 占位分支
    await pump(tester, dc);

    final bytes = await _capture(tester);
    final w = 800;
    expect(_nearWhite(_px(bytes, w, 400, 60)), isTrue,
        reason: '空条应画居中播放头（x=400）');
    expect(_nearTransparent(_px(bytes, w, 80, 60)), isTrue,
        reason: '无曲目：播放头外的条区应透明');
  });

  testWidgets('playhead stays centered near track end (P13, no tail clamp)', (tester) async {
    final dc = DeckController(0);
    _loudWave(dc);
    dc.duration.value = 329.0;
    dc.playhead.value = 320.0; // 接近曲尾
    await pump(tester, dc);

    final bytes = await _capture(tester);
    final w = 800;
    // P13：winStart = 320-30 = 290，指针恒居中 x=400
    expect(_nearWhite(_px(bytes, w, 400, 60)), isTrue,
        reason: '曲尾附近指针仍应居中（x=400）');
    expect(_bandColor(_px(bytes, w, 20, 60)), isTrue,
        reason: '窗口左缘（290s）应有波形内容');
    // 曲长 329 → 波形止于 x = (329-290)/60*800 ≈ 520，右段越界留白
    expect(_nearTransparent(_px(bytes, w, 720, 60)), isTrue,
        reason: '窗口越过曲尾 → 右段应留白（透明）');
  });

  testWidgets('track end: playhead fixed at center, trailing blank (P13)', (tester) async {
    final dc = DeckController(0);
    _loudWave(dc);
    dc.duration.value = 329.0;
    dc.playhead.value = 329.0; // 曲尾（引擎冻结在 duration 附近）
    dc.playing.value = false; // 自然播完已自动停止
    await pump(tester, dc);

    final bytes = await _capture(tester);
    final w = 800;
    // P13：winStart = 329-30 = 299；波形止于 x = (329-299)/60*800 = 400，
    // 右半留白；指针恒居中（不再停右缘）
    expect(_nearWhite(_px(bytes, w, 400, 60)), isTrue,
        reason: '曲尾停止后指针仍应居中（x=400）');
    expect(_bandColor(_px(bytes, w, 200, 60)), isTrue,
        reason: '中线左侧（[299, 329] 段）仍有波形');
    expect(_nearTransparent(_px(bytes, w, 799, 60)), isTrue,
        reason: '窗口越过曲尾 → 右半留白（透明）');
  });
}
