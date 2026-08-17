// 波形显示模式测试：RGB（mixxx 风格）与三色（bands）两种渲染路径 + overview 顶部越界修复。
// 像素级断言用 RepaintBoundary.toImage；未绘制处为全透明 (0,0,0,0)。

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

/// 满刻度、各频段不同电平：low=255 / mid=170 / high=110。
/// RGB 模式归一化混色 → 非白橙色 (255,170,110)；三色模式 → 三带可见。
void _loudWave(DeckController dc) {
  final bytes = Uint8List(125000 * 8);
  const pattern = [255, 255, 170, 170, 110, 110, 255, 255];
  for (var i = 0; i < 125000; i++) {
    bytes.setRange(i * 8, i * 8 + 8, pattern);
  }
  dc.wave.full = WaveData(bytes);
}

Future<Uint8List> _capture(WidgetTester tester, Finder boundaryFinder) async {
  final boundary =
      tester.renderObject<RenderRepaintBoundary>(boundaryFinder.first);
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

bool _transparent(List<int> p) => p[3] < 12;
bool _nearWhite(List<int> p) => p[0] > 200 && p[1] > 200 && p[2] > 200;
bool _opaqueColor(List<int> p) => p[3] > 200 && !_nearWhite(p);

/// 该列最顶（y 最小）非透明行；全透明返回 -1。
int _topRow(Uint8List bytes, int w, int h, int x) {
  for (var y = 0; y < h; y++) {
    if (bytes[(y * w + x) * 4 + 3] > 12) return y;
  }
  return -1;
}

void main() {
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

  final stripBoundary = find.descendant(
      of: find.byType(WaveStrip), matching: find.byType(RepaintBoundary));

  setUp(() {
    EngineController.instance.waveMode.value = WaveDisplayMode.rgb;
  });
  tearDown(() {
    EngineController.instance.waveMode.value = WaveDisplayMode.rgb;
  });

  testWidgets('RGB 模式：归一化混色竖条（非白、R>G>B）', (tester) async {
    final dc = DeckController(0);
    _loudWave(dc);
    dc.duration.value = 329.0;
    dc.playhead.value = 100.0;
    await pumpStrip(tester, dc);

    final bytes = await _capture(tester, stripBoundary);
    // 窗口内内容像素：(720,95) 中心行。归一化 color=(255,170,110)/255 → R>G>B。
    final p = _px(bytes, 800, 720, 95);
    expect(p[3] > 200, isTrue, reason: 'RGB 竖条应不透明');
    expect(p[0] > p[1] && p[1] > p[2], isTrue,
        reason: '归一化混色：R=low=255>G=mid=170>B=high=110，实际 $p');
    expect(_nearWhite(p), isFalse, reason: '非白（各频段不等）');
  });

  testWidgets('RGB 与三色模式在同一像素渲染不同', (tester) async {
    final dc = DeckController(0);
    _loudWave(dc);
    dc.duration.value = 329.0;
    dc.playhead.value = 100.0;

    EngineController.instance.waveMode.value = WaveDisplayMode.rgb;
    await pumpStrip(tester, dc);
    final rgb = await _capture(tester, stripBoundary);

    EngineController.instance.waveMode.value = WaveDisplayMode.bands;
    await pumpStrip(tester, dc);
    final bands = await _capture(tester, stripBoundary);

    final rgbPx = _px(rgb, 800, 720, 60);
    final bandsPx = _px(bands, 800, 720, 60);
    expect(rgbPx, isNot(equals(bandsPx)), reason: '两模式渲染应不同：rgb=$rgbPx bands=$bandsPx');
    // 两模式都应有波形内容 + 居中白色播放头
    expect(_opaqueColor(rgbPx), isTrue, reason: 'rgb 模式有内容');
    expect(_opaqueColor(bandsPx), isTrue, reason: 'bands 模式有内容');
    expect(_nearWhite(_px(bands, 800, 400, 60)), isTrue, reason: 'bands 播放头居中');
  });

  testWidgets('3-bands 切片：播放头仍居中、内容仍在', (tester) async {
    final dc = DeckController(0);
    _loudWave(dc);
    dc.duration.value = 329.0;
    dc.playhead.value = 100.0;
    EngineController.instance.waveMode.value = WaveDisplayMode.bands;
    await pumpStrip(tester, dc);

    final bytes = await _capture(tester, stripBoundary);
    expect(_nearWhite(_px(bytes, 800, 400, 60)), isTrue, reason: '播放头居中');
    // 满刻度 low=255/mid=170/high=110：柱内切片，中心行 red(low) 占大半 → 非白不透明
    expect(_opaqueColor(_px(bytes, 800, 720, 60)), isTrue, reason: '右侧有波形内容');
    expect(_opaqueColor(_px(bytes, 800, 20, 60)), isTrue, reason: '左侧有波形内容');
  });

  testWidgets('共享轮廓：RGB 与 3-bands 每列最顶不透明行一致', (tester) async {
    final dc = DeckController(0);
    // 线性幅度斜坡（8 字段同步变化），覆盖整个窗口，使轮廓逐列变化
    final bytes = Uint8List(125000 * 8);
    for (var i = 0; i < 125000; i++) {
      final v = (i * 255) ~/ 125000;
      for (var f = 0; f < 8; f++) {
        bytes[i * 8 + f] = v;
      }
    }
    dc.wave.full = WaveData(bytes);
    dc.duration.value = 329.0;
    dc.playhead.value = 100.0;

    EngineController.instance.waveMode.value = WaveDisplayMode.rgb;
    await pumpStrip(tester, dc);
    final rgb = await _capture(tester, stripBoundary);

    EngineController.instance.waveMode.value = WaveDisplayMode.bands;
    await pumpStrip(tester, dc);
    final bands = await _capture(tester, stripBoundary);

    const w = 800;
    final h = rgb.length ~/ (w * 4);
    for (var x = 0; x < w; x += 4) {
      final tRgb = _topRow(rgb, w, h, x);
      final tBands = _topRow(bands, w, h, x);
      expect((tRgb - tBands).abs() <= 1, isTrue,
          reason: 'x=$x 轮廓顶行应一致：rgb=$tRgb bands=$tBands');
    }
  });

  testWidgets('尖刺抑制：孤立高能量列被压到邻居高度', (tester) async {
    final dc = DeckController(0);
    // 背景安静（8 字段 ≈ 20），窗口内放一个单列尖刺（全 255）
    final bytes = Uint8List(125000 * 8);
    for (var i = 0; i < 125000; i++) {
      final v = i == 30000 ? 255 : 20;
      for (var f = 0; f < 8; f++) {
        bytes[i * 8 + f] = v;
      }
    }
    dc.wave.full = WaveData(bytes);
    dc.duration.value = 329.0;
    dc.playhead.value = 100.0; // winStart=70s → 窗口列 26250..48750，尖刺 30000 在窗口内
    await pumpStrip(tester, dc); // setUp 默认 rgb

    final bytesOut = await _capture(tester, stripBoundary);
    const w = 800;
    final h = bytesOut.length ~/ (w * 4);
    // 尖刺 30000 → 像素列 x=(30000-26250)/(60·375/800)=133.33 → x=133
    final tSpike = _topRow(bytesOut, w, h, 133);
    final tNeighbor = _topRow(bytesOut, w, h, 132);
    expect(tSpike, greaterThanOrEqualTo(tNeighbor - 1),
        reason: '尖刺列顶部不应高于邻居（被压平）：spike=$tSpike neigh=$tNeighbor');
  });

  testWidgets('overview：三带堆叠缩放到 ≤maxH，顶部不越界', (tester) async {
    final dc = DeckController(0);
    // 三带全 255：旧实现单列 Σ=6×255 堆叠溢出顶部，行 0..3 会有内容
    dc.wave.full =
        WaveData(Uint8List.fromList(List.filled(125000 * 8, 255)));
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
    final bytes = await _capture(tester, boundary);
    const w = 400;
    // 远离播放头(x≈0)的列：顶部 4 行应透明（缩放后栈顶 ≥ maxH 余量 6px）
    for (var x = 20; x < w; x += 40) {
      for (var y = 0; y < 4; y++) {
        expect(_transparent(_px(bytes, w, x, y)), isTrue,
            reason: '($x,$y) 应透明：列堆叠缩放到 ≤maxH，顶部不越界');
      }
    }
  });
}
