//! ManualLoop widget 测试（PadActions 注入，不碰桥）：中间按钮激活/取消、
//! ÷2/×2 本地拍数 + 激活中立即重设、In/Out 写 loop 总线并激活。
//!
//! P22-D：_toggle/_setIn/_setOut 已改 onTapDown（按下即触发，不等待手势
//! 仲裁）——`tester.tap` 含按下+抬起，用例与断言均不变，语义注释见
//! manual_loop.dart 头部。

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/deck_controller.dart';
import 'package:hypermixx/widgets/deck_pads.dart';
import 'package:hypermixx/widgets/manual_loop.dart';

/// 记录调用的假动作出口。
class _FakeActions extends PadActions {
  final loops = <(String, double)>[];
  final loopActive = <bool>[];
  final beatLoops = <double>[];

  @override
  void setLoopIn(int deck, double seconds) => loops.add(('in', seconds));
  @override
  void setLoopOut(int deck, double seconds) => loops.add(('out', seconds));
  @override
  void setLoopActive(int deck, bool on) => loopActive.add(on);
  @override
  void activateBeatLoop(int deck, double beats) => beatLoops.add(beats);
}

Widget _wrap(DeckController dc, _FakeActions a) {
  return MaterialApp(
    home: Scaffold(
      backgroundColor: const Color(0xFF1A1E24),
      body: Center(
        child: SizedBox(width: 200, height: 80, child: ManualLoop(deck: dc, actions: a)),
      ),
    ),
  );
}

void main() {
  test('fmtBeats：只显示分数或整数（P20）', () {
    // 整数（含浮点噪声）
    expect(fmtBeats(4.0), '4');
    expect(fmtBeats(4.0000001), '4', reason: '去总线折算噪声');
    expect(fmtBeats(64.0), '64');
    // ≤32 分母分数
    expect(fmtBeats(0.5), '1/2');
    expect(fmtBeats(0.25), '1/4');
    expect(fmtBeats(0.125), '1/8');
    expect(fmtBeats(1 / 32), '1/32');
    expect(fmtBeats(0.75), '3/4');
    expect(fmtBeats(1.5), '3/2');
    // 任意 ≤32 分母分数（手动 In/Out 的任意长度）
    expect(fmtBeats(0.7), '7/10');
    expect(fmtBeats(0), '0');
  });

  testWidgets('中间按钮：未激活点击 → beatloop 4 拍；激活中点击 → 取消', (tester) async {
    final dc = DeckController(0);
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    expect(find.text('4'), findsOneWidget);
    await tester.tap(find.text('4'));
    expect(a.beatLoops, [4.0], reason: '未激活点击 = 激活 beatloop 默认拍数');
    expect(a.loopActive, isEmpty);

    dc.loopActive.value = true;
    dc.loopIn.value = 0;
    dc.loopOut.value = 2; // 120BPM 下 2s = 4 拍
    dc.bpm.value = 120;
    await tester.pump();
    expect(find.text('4'), findsOneWidget, reason: '激活中显示实际环拍数');
    await tester.tap(find.text('4'));
    expect(a.loopActive, [false], reason: '激活中点击 = 取消');
  });

  testWidgets('激活中显示实际环拍数（bpm 折算），未激活显示目标拍数', (tester) async {
    final dc = DeckController(0);
    dc.bpm.value = 120;
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));
    expect(find.text('4'), findsOneWidget);

    // 激活环 0.5s..1.5s @120BPM = 2 拍
    dc.loopActive.value = true;
    dc.loopIn.value = 0.5;
    dc.loopOut.value = 1.5;
    await tester.pump();
    expect(find.text('2'), findsOneWidget);
  });

  testWidgets('÷2/×2 改本地拍数；激活中立即按新拍数重设 beatloop', (tester) async {
    final dc = DeckController(0);
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    await tester.tap(find.text('×2'));
    await tester.pump();
    expect(find.text('8'), findsOneWidget);
    expect(a.beatLoops, isEmpty, reason: '未激活只改本地拍数');

    dc.loopActive.value = true;
    await tester.pump();
    await tester.tap(find.text('÷2'));
    expect(a.beatLoops, [4.0], reason: '激活中 ÷2 = 立即重设 beatloop');
    await tester.pump();
    expect(find.text('4'), findsOneWidget);
  });

  testWidgets('P23 In：只写 loop_in raw 秒数，不激活、不回填 out', (tester) async {
    final dc = DeckController(0);
    dc.playhead.value = 31.5;
    dc.bpm.value = 120;
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    await tester.tap(find.text('In'));
    expect(a.loops, [('in', 31.5)], reason: 'P23：In 只定下界（raw 秒数）');
    expect(a.loopActive, isEmpty, reason: 'P23：In 不激活，由 Out 定上界并激活');
  });

  testWidgets('P23 In：已有有效 out 不动；Out：只写原始播放头秒数 + 激活',
      (tester) async {
    final dc = DeckController(0);
    dc.playhead.value = 10;
    dc.bpm.value = 120;
    dc.loopOut.value = 15; // 已有有效环（out > in）
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    await tester.tap(find.text('In'));
    expect(a.loops, [('in', 10.0)], reason: 'out 有效时不动 out');
    expect(a.loopActive, isEmpty, reason: 'In 不激活');

    // Out：playhead = 10 → 只写 raw 10.0（量化交给引擎 snap_loop_bounds）
    a.loops.clear();
    dc.playhead.value = 10;
    dc.loopIn.value = 5; // 已有有效 in（in < out）
    await tester.tap(find.text('Out'));
    expect(a.loops, [('out', 10.0)], reason: 'P23：out = raw 播放头秒数');
    expect(a.loopActive, [true], reason: 'Out 确定上界后激活');
  });

  testWidgets('P23 Out：raw 秒数透传，不量化（引擎负责 snap）', (tester) async {
    final dc = DeckController(0);
    dc.bpm.value = 120; // 拍长 0.5s（Flutter 侧不再使用）
    dc.loopIn.value = 10;
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    // 11.8s（3.6 拍，P21 会取整到 12.0）→ P23 原样传 11.8
    dc.playhead.value = 11.8;
    await tester.tap(find.text('Out'));
    expect(a.loops, [('out', 11.8)], reason: 'P23：不做整拍取整');
    expect(a.loopActive, [true]);

    // 10.14s（0.28 拍，P21 保底 1 拍 → 10.5）→ P23 原样传 10.14
    a.loops.clear();
    a.loopActive.clear();
    dc.playhead.value = 10.14;
    await tester.tap(find.text('Out'));
    expect(a.loops, [('out', 10.14)], reason: 'P23：不做保底拍数');
  });

  testWidgets('P23 Out：无有效 in 也不回拉（起点回拉归引擎）', (tester) async {
    final dc = DeckController(0);
    dc.playhead.value = 20;
    dc.bpm.value = 120;
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    await tester.tap(find.text('Out'));
    expect(a.loops, [('out', 20.0)],
        reason: 'P23：只写 out raw 秒数，起点回拉由引擎 snap_loop_bounds 做');
    expect(a.loopActive, [true]);
  });
}
