// 实机打击垫交互：真桥 + 真引擎（deck1 = decks[0]，先渲染）。
// 载入 12s 正弦 WAV → 分析完成后写死 grid 120BPM（offset 0）→
// CUE（落点/试听/播放中暂停回点）→ LOOP（激活/取消）→ BEATJUMP（跳拍/翻页）→
// FX（P18.1 只占位，功能并入 DeckFx）→ HOTCUE（落点/删除）。
//
// 注意：60Hz tick 持续调度帧 → 禁用 pumpAndSettle；引擎在真实线程里
// 按真实时间播放 → 所有"等到某状态"都用 DateTime 截止 + 短 pump 轮询。

import 'dart:io';
import 'dart:math' as math;
import 'dart:typed_data';

import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

import 'package:hypermixx/engine/engine_controller.dart';
import 'package:hypermixx/main.dart' as app;
import 'package:hypermixx/src/rust/api.dart';
import 'package:hypermixx/widgets/deck_pads.dart';
import 'package:hypermixx/widgets/deck_panel.dart';

/// 截止轮询：每 50ms 一 pump（让引擎线程/事件循环推进），真实时间超时即失败。
Future<void> _waitUntil(
  WidgetTester tester,
  bool Function() cond, {
  String reason = '条件超时',
  Duration timeout = const Duration(seconds: 10),
}) async {
  final deadline = DateTime.now().add(timeout);
  while (!cond()) {
    if (DateTime.now().isAfter(deadline)) fail(reason);
    await tester.pump(const Duration(milliseconds: 50));
  }
}

/// deck1 打击垫区内的文本。
Finder _pad(String label) => find.descendant(
    of: find.byType(DeckPads).first, matching: find.text(label));

Finder _tab(String label) => find.descendant(
    of: find.byType(DeckPads).first, matching: find.text(label));

/// deck1 的 CUE 按钮。
final _cue = find.descendant(
    of: find.byType(DeckPanel).first, matching: find.text('CUE'));

/// 48kHz 单声道 16-bit 正弦 WAV（12s，440Hz）。
Future<String> _makeSineWav() async {
  const sr = 48000;
  const secs = 12;
  const freq = 440.0;
  final n = sr * secs;
  final dataSize = n * 2;
  final bytes = ByteData(44 + dataSize);
  void putStr(int off, String s) {
    for (var i = 0; i < s.length; i++) {
      bytes.setUint8(off + i, s.codeUnitAt(i));
    }
  }

  putStr(0, 'RIFF');
  bytes.setUint32(4, 36 + dataSize, Endian.little);
  putStr(8, 'WAVE');
  putStr(12, 'fmt ');
  bytes.setUint32(16, 16, Endian.little); // fmt chunk
  bytes.setUint16(20, 1, Endian.little); // PCM
  bytes.setUint16(22, 1, Endian.little); // mono
  bytes.setUint32(24, sr, Endian.little);
  bytes.setUint32(28, sr * 2, Endian.little); // byte rate
  bytes.setUint16(32, 2, Endian.little); // block align
  bytes.setUint16(34, 16, Endian.little); // bits
  putStr(36, 'data');
  bytes.setUint32(40, dataSize, Endian.little);
  for (var i = 0; i < n; i++) {
    final v = (math.sin(2 * math.pi * freq * i / sr) * 0.6 * 32767).round();
    bytes.setInt16(44 + i * 2, v, Endian.little);
  }
  final dir = await Directory.systemTemp.createTemp('hypermixx_it_');
  addTearDown(() => dir.delete(recursive: true));
  final f = File('${dir.path}/sine.wav');
  await f.writeAsBytes(bytes.buffer.asUint8List());
  return f.path;
}

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('deck pads: CUE / LOOP / BEATJUMP / FX / HOTCUE 全链路',
      (tester) async {
    tester.view.physicalSize = const Size(1600, 2200);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.reset);

    final wav = await _makeSineWav();

    await app.main(); // 载桥 + 启动引擎
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));

    final dc = EngineController.instance.decks[0];
    expect(find.byType(DeckPads), findsNWidgets(2), reason: '两个 deck 各一个 pad 区');

    // 载曲并等分析 Done（durationFrames 由 Done 事件填充）。
    EngineController.instance.loadTrackInto(0, wav);
    await _waitUntil(tester, () => dc.wave.durationFrames > 0,
        reason: '分析未在期限内完成', timeout: const Duration(seconds: 30));

    // UI 控件读的是 dc 状态（60Hz tick 分发，比总线晚 ≤16ms），等待条件必须
    // 以 dc 为准——否则控件可能读到滞后值（如播头还是 0 就把 CUE 判成"位于点位试听"）。
    double getPh() => dc.playhead.value;
    bool getPlay() => dc.playing.value;

    // Done 后写死 grid：120BPM、偏移 0（分析可能覆盖，故在其后设置）。
    busSet(path: 'Deck1.grid_bpm', value: 120);
    busSet(path: 'Deck1.grid_offset', value: 0);

    // 引擎载曲后自动开播（deck.load() → playing=true）——CUE 落点/试听语义
    // 要求停播起点，显式停播并等 UI 状态同步（否则 CUE 按下走"召回"分支）。
    busSet(path: 'Deck1.play', value: 0);
    await _waitUntil(tester, () => !getPlay(), reason: '停播失败（载曲自动播放）');

    // ---- CUE：停播远离 → 落点 ----
    seekExact(deck: 0, seconds: 1.3);
    await _waitUntil(tester, () => (getPh() - 1.3).abs() < 0.1,
        reason: 'seek 1.3 未到位');
    await tester.tap(_cue);
    await tester.pump();
    final cue = dc.cuePoint.value;
    expect(cue, isNotNull);
    expect((cue! - 1.3).abs() < 0.1, isTrue,
        reason: 'cue 应落在播头附近（keylock 稳态延迟 0.012s），实际 $cue');
    expect(getPlay(), isFalse, reason: '落点不改变播放状态');

    // ---- CUE：停播位于 cue 点 → 按住试听、松开停播回点 ----
    seekExact(deck: 0, seconds: cue);
    await _waitUntil(tester, () => (getPh() - cue).abs() < 0.05,
        reason: '回 cue 点未到位（试听前置）');
    final g = await tester.startGesture(tester.getCenter(_cue));
    await _waitUntil(tester, getPlay, reason: '按住 CUE 未开始播放');
    await g.up();
    await _waitUntil(
        tester, () => !getPlay() && (getPh() - cue).abs() < 0.05,
        reason: '松开 CUE 未停播回点');

    // ---- CUE：播放中 → P19 暂停并回 cue 点（原：召回继续播）----
    busSet(path: 'Deck1.play', value: 1);
    await _waitUntil(tester, () => getPh() > cue + 0.15,
        reason: '播放未推进', timeout: const Duration(seconds: 5));
    final before = getPh();
    await tester.tap(_cue);
    await _waitUntil(tester, () => (getPh() - cue).abs() < 0.1,
        reason: '点击 CUE 未回到 cue 点');
    expect(getPh(), lessThan(before - 0.05), reason: '应显著回退');
    expect(getPlay(), isFalse, reason: 'P19：播放中点击 CUE 应暂停');

    // ---- LOOP：1 拍激活（量化后 in/out 差 0.5s@120）→ 再点取消 ----
    await tester.tap(_tab('LOOP'));
    await tester.pump();
    await tester.tap(_pad('1'));
    await _waitUntil(tester, () => busGet(path: 'Deck1.loop_active') > 0.5,
        reason: 'loop 未激活');
    final li = busGet(path: 'Deck1.loop_in');
    final lo = busGet(path: 'Deck1.loop_out');
    expect((lo - li - 0.5).abs() < 0.02, isTrue,
        reason: '1 拍 loop 长度应 ≈0.5s，实际 ${lo - li}');
    // 等 tick 把 loop 状态刷进 dc（pad 激活态判定用）
    await _waitUntil(tester, () => dc.loopActive.value, reason: 'loop 快照未刷新');
    await tester.tap(_pad('1'));
    await _waitUntil(tester, () => busGet(path: 'Deck1.loop_active') < 0.5,
        reason: 'loop 未取消');

    // ---- BEATJUMP：4 拍 = +2.0s（简单加减，匹配当前速度）----
    await tester.tap(_tab('BEATJUMP'));
    await tester.pump();
    seekExact(deck: 0, seconds: 2.0);
    await _waitUntil(tester, () => (getPh() - 2.0).abs() < 0.1,
        reason: 'seek 2.0 未到位');
    await tester.tap(_pad('▶4'));
    await _waitUntil(tester, () => (getPh() - 4.0).abs() < 0.12,
        reason: '▶4 未跳到 4.0');
    // 翻页：p1 = 左右 16/32
    await tester.tap(find.descendant(
        of: find.byType(DeckPads).first,
        matching: find.byIcon(Icons.keyboard_arrow_down)));
    await tester.pump();
    expect(_pad('◀16'), findsOneWidget);
    expect(_pad('▶32'), findsOneWidget);

    // ---- FX：P18.1 只占位（功能并入 DeckFx 单通道）----
    await tester.tap(_tab('FX'));
    await tester.pump();
    final fxEnableBefore = busGet(path: 'Deck1.fx1_enable');
    final pads = find
        .descendant(of: find.byType(DeckPads).first, matching: find.text('FX'));
    expect(pads, findsNWidgets(9), reason: '1 选项卡 + 8 占位 pad');
    await tester.tap(pads.first);
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    await tester.tap(pads.first, buttons: kSecondaryMouseButton);
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    expect(busGet(path: 'Deck1.fx1_enable'), fxEnableBefore,
        reason: '占位 pad 点击/右键不写 enable');

    // ---- HOTCUE：停播落点 → × 删除 ----
    await tester.tap(_tab('HOTCUE'));
    await tester.pump();
    seekExact(deck: 0, seconds: 5.0);
    await _waitUntil(tester, () => (getPh() - 5.0).abs() < 0.1,
        reason: 'seek 5.0 未到位');
    await tester.tap(_pad('1'));
    await tester.pump();
    expect(dc.hotcues[0].value, isNotNull);
    expect((dc.hotcues[0].value! - 5.0).abs() < 0.1, isTrue,
        reason: 'hotcue 1 应落在 5.0 附近');
    await tester.tap(find.descendant(
        of: find.byType(DeckPads).first, matching: find.byIcon(Icons.close)));
    await tester.pump();
    expect(dc.hotcues[0].value, isNull, reason: '× 应删除 hotcue');
  });
}
