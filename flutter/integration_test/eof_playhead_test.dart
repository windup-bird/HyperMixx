// 播完行为复现（bug：滚动波形到结束时仍右移）：
// 真桥 + 真引擎 + 12s 正弦 WAV，seek 到结尾前 1.5s 播放到自然停止，
// 断言停止后 playhead 冻结（不再右移）且不超过 duration。
//
// 注意：60Hz tick 持续调度帧 → 禁用 pumpAndSettle；引擎在真实线程里
// 按真实时间播放 → 用 DateTime 截止 + 短 pump 轮询。

import 'dart:io';
import 'dart:math' as math;
import 'dart:typed_data';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

import 'package:hypermixx/engine/engine_controller.dart';
import 'package:hypermixx/main.dart' as app;
import 'package:hypermixx/src/rust/api.dart';

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

  testWidgets('EOF：播完 playhead 冻结在曲尾，停止后不再右移', (tester) async {
    tester.view.physicalSize = const Size(1600, 2200);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.reset);

    final wav = await _makeSineWav();

    await app.main(); // 载桥 + 启动引擎
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));

    final dc = EngineController.instance.decks[0];
    EngineController.instance.loadTrackInto(0, wav);
    await _waitUntil(tester, () => dc.wave.durationFrames > 0,
        reason: '分析未在期限内完成', timeout: const Duration(seconds: 30));

    // 载曲后自动开播 → 停播，seek 到结尾前 1.5s，再播到自然停止。
    busSet(path: 'Deck1.play', value: 0);
    await _waitUntil(tester, () => !dc.playing.value, reason: '停播失败');
    seekExact(deck: 0, seconds: 10.5);
    await _waitUntil(tester, () => (dc.playhead.value - 10.5).abs() < 0.1,
        reason: 'seek 10.5 未到位');
    busSet(path: 'Deck1.play', value: 1);
    await _waitUntil(tester, () => dc.playing.value, reason: '播放未开始');
    await _waitUntil(tester, () => !dc.playing.value,
        reason: '未自然停止', timeout: const Duration(seconds: 6));

    final frozen = dc.playhead.value;
    final dur = dc.duration.value;
    debugPrint('EOF: frozen_ph=$frozen duration=$dur');

    // 停止后继续采样 1.5s：playhead 必须冻结（bug 现象 = 持续右移）
    for (var i = 0; i < 30; i++) {
      await tester.pump(const Duration(milliseconds: 50));
      final ph = dc.playhead.value;
      expect((ph - frozen).abs() < 1e-6, isTrue,
          reason: '停止后 playhead 不应继续推进：frozen=$frozen ph=$ph (i=$i)');
    }
    expect(frozen <= dur + 0.05, isTrue,
        reason: '播头不应越过曲尾：frozen=$frozen dur=$dur');
  });
}
