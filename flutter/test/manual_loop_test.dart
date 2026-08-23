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
  final loops = <String>[];
  final loopActive = <bool>[];
  final beatLoops = <double>[];

  @override
  void loopInAtPlayhead(int deck) => loops.add('in');
  @override
  void loopOutAtPlayhead(int deck) => loops.add('out');
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
        child: SizedBox(
          width: 200,
          height: 80,
          child: ManualLoop(deck: dc, actions: a),
        ),
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
    dc.loopOut.value = 2;
    dc.loopBeats.value = 4;
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

    // 引擎快照给出 2 拍。
    dc.loopActive.value = true;
    dc.loopBeats.value = 2;
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
    expect(a.loops, ['in'], reason: 'In 命令由 Rust 侧捕获播放头并量化');
    expect(a.loopActive, isEmpty, reason: 'In 不激活，由 Out 定上界并激活');
  });

  testWidgets('In/Out：只下发有序的 Rust 音频线程命令', (tester) async {
    final dc = DeckController(0);
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    await tester.tap(find.text('In'));
    await tester.tap(find.text('Out'));
    expect(a.loops, ['in', 'out']);
    expect(a.loopActive, isEmpty, reason: '激活由 Rust 收到有效 Out 后完成');
  });
}
