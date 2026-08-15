//! BeatJumpPanel widget 测试（PadActions 注入，不碰桥）：÷2/×2 本地跳拍数、
//! 中间点击回默认、◀/▶ onTapDown 立即跳拍（方向 × 拍数）。

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/deck_controller.dart';
import 'package:hypermixx/widgets/beatjump_panel.dart';
import 'package:hypermixx/widgets/deck_pads.dart';

/// 记录调用的假动作出口。
class _FakeActions extends PadActions {
  final jumps = <double>[];

  @override
  void beatJump(int deck, double beats) => jumps.add(beats);
}

Widget _wrap(DeckController dc, _FakeActions a) {
  return MaterialApp(
    home: Scaffold(
      backgroundColor: const Color(0xFF1A1E24),
      body: Center(
        child: SizedBox(
          width: 200,
          height: 80,
          child: BeatJumpPanel(deck: dc, actions: a),
        ),
      ),
    ),
  );
}

void main() {
  testWidgets('◀/▶ onTapDown 立即跳 ±4（默认 4 拍）', (tester) async {
    final dc = DeckController(0);
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));
    expect(find.text('±4'), findsOneWidget);

    await tester.tap(find.text('▶'));
    expect(a.jumps, [4.0]);
    await tester.tap(find.text('◀'));
    expect(a.jumps, [4.0, -4.0]);
  });

  testWidgets('÷2/×2 改跳拍数（整数域，clamp 1..32），◀/▶ 用新拍数', (tester) async {
    final dc = DeckController(0);
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    await tester.tap(find.text('×2'));
    await tester.pump();
    expect(find.text('±8'), findsOneWidget);
    await tester.tap(find.text('▶'));
    expect(a.jumps.last, 8.0);

    await tester.tap(find.text('÷2'));
    await tester.tap(find.text('÷2'));
    await tester.tap(find.text('÷2'));
    await tester.pump();
    expect(find.text('±1'), findsOneWidget, reason: '÷2 下限 1（4→2→1→1）');
    await tester.tap(find.text('◀'));
    expect(a.jumps.last, -1.0);

    for (var i = 0; i < 6; i++) {
      await tester.tap(find.text('×2'));
      await tester.pump();
    }
    expect(find.text('±32'), findsOneWidget, reason: '×2 上限 32');
  });

  testWidgets('中间按钮点击回默认 4 拍', (tester) async {
    final dc = DeckController(0);
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    await tester.tap(find.text('×2'));
    await tester.pump();
    expect(find.text('±8'), findsOneWidget);
    await tester.tap(find.text('±8'));
    await tester.pump();
    expect(find.text('±4'), findsOneWidget);
    expect(a.jumps, isEmpty, reason: '显示按钮不跳拍');
  });
}
