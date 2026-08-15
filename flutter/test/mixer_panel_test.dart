//! MixerPanel widget 测试：两侧旋钮列结构（标签/纵向顺序/计数）、
//! 中心推子区（Slider ×3、VU ×2）、无桥时 gain/filter 总线初始化回 0。

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/engine_controller.dart';
import 'package:hypermixx/widgets/mixer_knob.dart';
import 'package:hypermixx/widgets/mixer_panel.dart';

Widget _wrap() {
  return MaterialApp(
    home: Scaffold(
      backgroundColor: const Color(0xFF1A1E24),
      body: SizedBox(
        width: 300,
        height: 900,
        child: MixerPanel(),
      ),
    ),
  );
}

void main() {
  testWidgets('两侧旋钮列标签齐全（GAIN/EQ 三带/FILTER ×2、无 XFADE 文字）', (tester) async {
    await tester.pumpWidget(_wrap());
    await tester.pump();
    expect(find.text('DECK 1'), findsOneWidget);
    expect(find.text('DECK 2'), findsOneWidget);
    for (final t in ['GAIN', 'HIGH', 'MID', 'LOW', 'FILTER']) {
      expect(find.text(t), findsNWidgets(2), reason: t);
    }
    expect(find.text('XFADE'), findsNothing, reason: '交叉推子无文字');
    expect(tester.takeException(), isNull);
  });

  testWidgets('MixerKnob ×10、Slider ×3（2 音量 + 1 交叉）、VU ×2', (tester) async {
    await tester.pumpWidget(_wrap());
    await tester.pump();
    expect(find.byType(MixerKnob), findsNWidgets(10));
    expect(find.byType(Slider), findsNWidgets(3));
    expect(find.byType(LinearProgressIndicator), findsNWidgets(2));
  });

  testWidgets('无桥：gain/filter initFromBus 回 0（无异常、无值文本）', (tester) async {
    await tester.pumpWidget(_wrap());
    await tester.pump();
    expect(tester.takeException(), isNull);
    expect(find.text('0.0 dB'), findsNothing);
    expect(find.text('0.0'), findsNothing);
  });

  testWidgets('旋钮纵向顺序 gain→high→mid→low→filter', (tester) async {
    await tester.pumpWidget(_wrap());
    await tester.pump();
    // .first = 左列（deck1）；两列同序，任取一列断言
    double y(String label) => tester.getTopLeft(find.text(label).first).dy;
    expect(y('GAIN'), lessThan(y('HIGH')));
    expect(y('HIGH'), lessThan(y('MID')));
    expect(y('MID'), lessThan(y('LOW')));
    expect(y('LOW'), lessThan(y('FILTER')));
  });

  testWidgets('VU 条跟随 dc.vu 且位于两推子中间（deck0 0.7 / deck1 0.0）', (tester) async {
    final engine = EngineController.instance;
    final vu = engine.decks[0].vu;
    final prev = vu.value;
    addTearDown(() => vu.value = prev);
    vu.value = 0.7;
    await tester.pumpWidget(_wrap());
    await tester.pump();
    final bars = tester
        .widgetList<LinearProgressIndicator>(find.byType(LinearProgressIndicator))
        .toList();
    expect(bars.length, 2);
    expect(bars[0].value, closeTo(0.7, 1e-9));
    expect(bars[1].value, closeTo(0.0, 1e-9));
    // 横向顺序：推子0 < VU0 < VU1 < 推子1（树序 Slider = [推子0, 推子1, XFADE]）
    double cx(Finder f) => tester.getCenter(f).dx;
    expect(cx(find.byType(Slider).at(0)), lessThan(cx(find.byType(LinearProgressIndicator).at(0))));
    expect(cx(find.byType(LinearProgressIndicator).at(0)), lessThan(cx(find.byType(LinearProgressIndicator).at(1))));
    expect(cx(find.byType(LinearProgressIndicator).at(1)), lessThan(cx(find.byType(Slider).at(1))));
  });
}
