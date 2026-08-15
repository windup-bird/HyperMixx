//! MixerKnob widget 测试：主导轴拖拽计值、clamp、双击回 0、
//! initFromBus 桥缺失回 0、单行文字（平时标签 / 拖动中显值 / 双击闪值）。

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/widgets/mixer_knob.dart';

Widget _wrap(Widget child) {
  return MaterialApp(
    home: Scaffold(
      backgroundColor: const Color(0xFF1A1E24),
      body: Center(child: SizedBox(width: 200, height: 200, child: child)),
    ),
  );
}

/// 旋钮身（44×44 的 GestureDetector），而非整列（列中心在文字上）。
Finder _knobBody() => find.descendant(
    of: find.byType(MixerKnob), matching: find.byType(GestureDetector));

void main() {
  testWidgets('上/下拖动主导轴计值 + clamp', (tester) async {
    final calls = <double>[];
    await tester.pumpWidget(_wrap(MixerKnob(
      label: 'GAIN',
      min: -12,
      max: 12,
      onChanged: calls.add,
    )));
    // 拖旋钮身而非整列；test 手势吃 ~20px slop，
    // 故用超满量程的位移保证 clamp 边界精确成立（满量程 140px）
    final knob = _knobBody();
    await tester.drag(knob, const Offset(0, -200));
    expect(calls.last, closeTo(12.0, 1e-9));
    await tester.drag(knob, const Offset(0, 500));
    expect(calls.last, closeTo(-12.0, 1e-9));
    await tester.pump();
    // pan 结束回标签：值文本消失
    expect(find.text('-12.0'), findsNothing);
    expect(find.text('GAIN'), findsOneWidget);
    // 双击识别器的 tap 追踪计时器到期（否则 Pending timers 判失败）
    await tester.pump(const Duration(milliseconds: 700));
  });

  testWidgets('水平拖动计值 + clamp', (tester) async {
    final calls = <double>[];
    await tester.pumpWidget(_wrap(MixerKnob(
      label: 'F',
      min: -1,
      max: 1,
      onChanged: calls.add,
    )));
    // 右拖 35px（扣 slop ~20 → 有效 ~15px）：正方向、量级 <0.6
    final knob = _knobBody();
    await tester.drag(knob, const Offset(35, 0));
    expect(calls.last, greaterThan(0));
    expect(calls.last, lessThan(0.6));
    // 左拖 300px（扣 slop 后仍超满量程 140）→ −1（clamp）
    await tester.drag(knob, const Offset(-300, 0));
    expect(calls.last, closeTo(-1.0, 1e-9));
    await tester.pump();
    expect(find.text('-1.0'), findsNothing);
    expect(find.text('F'), findsOneWidget);
    // 双击识别器的 tap 追踪计时器到期（否则 Pending timers 判失败）
    await tester.pump(const Duration(milliseconds: 700));
  });

  testWidgets('双击回 0 + 闪值 600ms 后回标签', (tester) async {
    final calls = <double>[];
    await tester.pumpWidget(_wrap(MixerKnob(
      label: 'FILTER',
      min: -1,
      max: 1,
      onChanged: calls.add,
    )));
    final knob = _knobBody();
    await tester.drag(knob, const Offset(180, 0)); // 扣 slop 后 ≥140 → +1（clamp）
    expect(calls.last, closeTo(1.0, 1e-9));
    calls.clear();
    await tester.tap(knob);
    await tester.pump(const Duration(milliseconds: 60));
    await tester.tap(knob);
    await tester.pump(const Duration(milliseconds: 60));
    expect(calls, isNotEmpty, reason: '双击应触发 onChanged(0.0)');
    expect(calls.last, 0.0);
    // 闪值期：值文本替代标签
    expect(find.text('0.0'), findsOneWidget);
    expect(find.text('FILTER'), findsNothing);
    // 700ms 覆盖 600ms 闪值 timer + 识别器 timer → 回标签
    await tester.pump(const Duration(milliseconds: 700));
    expect(find.text('FILTER'), findsOneWidget);
    expect(find.text('0.0'), findsNothing);
  });

  testWidgets('initFromBus 桥缺失回 0（try/catch）', (tester) async {
    await tester.pumpWidget(_wrap(MixerKnob(
      label: 'GAIN',
      min: -12,
      max: 12,
      initFromBus: 'Deck1.gain',
      format: (v) => '${v.toStringAsFixed(1)} dB',
      onChanged: (_) {},
    )));
    expect(tester.takeException(), isNull);
    // 平时只显标签
    expect(find.text('GAIN'), findsOneWidget);
    expect(find.text('0.0 dB'), findsNothing);
    // 小位移拖动 → 显 dB 格式值且 |v| 小（证明初值 0 被总线初始化路径覆盖；
    // 首段 moveBy 只触发 pan 接受、不递增量，故分多段小步拖）
    final knob = _knobBody();
    final g = await tester.startGesture(tester.getCenter(knob));
    await tester.pump();
    await g.moveBy(const Offset(0, -10));
    await tester.pump();
    await g.moveBy(const Offset(0, -10));
    await tester.pump();
    final vt = tester.widget<Text>(find.textContaining('dB'));
    final v = double.parse(vt.data!.split(' ')[0]);
    expect(v.abs(), lessThan(3.0));
    await g.up();
    await tester.pump();
    expect(find.text('GAIN'), findsOneWidget);
    await tester.pump(const Duration(milliseconds: 700));
  });

  testWidgets('初始只显标签、拖动中显值', (tester) async {
    await tester.pumpWidget(_wrap(MixerKnob(
      label: 'GAIN',
      min: -12,
      max: 12,
      onChanged: (_) {},
    )));
    expect(find.text('GAIN'), findsOneWidget);
    expect(find.text('0.0'), findsNothing);
    final knob = _knobBody();
    final g = await tester.startGesture(tester.getCenter(knob));
    await tester.pump();
    // 首段 moveBy 只触发 pan 接受，故分多段：共 −200px → 超满量程 → +12（clamp）
    for (var i = 0; i < 10; i++) {
      await g.moveBy(const Offset(0, -20));
      await tester.pump();
    }
    expect(find.text('12.0'), findsOneWidget);
    expect(find.text('GAIN'), findsNothing);
    await g.up();
    await tester.pump();
    expect(find.text('GAIN'), findsOneWidget);
    await tester.pump(const Duration(milliseconds: 700));
  });
}
