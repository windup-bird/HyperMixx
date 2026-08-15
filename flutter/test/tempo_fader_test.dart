//! TempoFader widget 测试（回调注入，不碰桥）：微调按住重复、垂直推子
//! 拖拽计值、默认路径渲染。
//! P19：nudge 键已移到 transport 行（TransportRow，见 transport_row_test）。

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/deck_controller.dart';
import 'package:hypermixx/widgets/tempo_fader.dart';

Widget _wrap(
  DeckController dc, {
  void Function(double v)? onSetRate,
}) {
  return MaterialApp(
    home: Scaffold(
      backgroundColor: const Color(0xFF1A1E24),
      body: Center(
        child: SizedBox(
          width: 72,
          height: 200,
          child: TempoFader(deck: dc, onSetRate: onSetRate),
        ),
      ),
    ),
  );
}

void main() {
  testWidgets('微调按住立即 + 每 100ms 重复、松开停', (tester) async {
    final dc = DeckController(0);
    final calls = <double>[];
    await tester.pumpWidget(_wrap(dc, onSetRate: calls.add));

    final g = await tester.startGesture(tester.getCenter(find.text('−')));
    await tester.pump();
    expect(calls, [-0.5], reason: '按住立即一次（rate 0 − 0.5）');
    await tester.pump(const Duration(milliseconds: 100));
    await tester.pump(const Duration(milliseconds: 100));
    expect(calls.length, 3, reason: '每 100ms 重复');
    await g.up();
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 300));
    expect(calls.length, 3, reason: '松开后 timer 停止');
  });

  testWidgets('拖拽垂直推子计值（上快下慢、clamp ±8）', (tester) async {
    final dc = DeckController(0);
    final calls = <double>[];
    await tester.pumpWidget(_wrap(dc, onSetRate: calls.add));
    // 推子区域高 170（200 − 8 间距上 − 22 按钮行下）；中点(y=100)拖到顶
    // 需上移 100 → t=0 → +8；拖到底需下移 70 → dy=170 → t=1 → −8
    final center = tester.getCenter(find.byType(TempoFader));
    await tester.dragFrom(center, const Offset(0, -100));
    expect(calls.last, closeTo(8.0, 0.5), reason: '拖到顶 = +8%');
    await tester.dragFrom(center, const Offset(0, 70));
    expect(calls.last, closeTo(-8.0, 0.5), reason: '拖到底 = −8%');
    await tester.dragFrom(center, const Offset(0, -200));
    expect(calls.last, closeTo(8.0, 1e-9), reason: '越界 clamp 到 +8');
    // 双击识别器的 tap 追踪计时器到期（否则 Pending timers 判失败）
    await tester.pump(const Duration(milliseconds: 700));
  });

  testWidgets('同步期间拖拽写 bus（引擎软接管判定）、微调 gate、thumb 恒显示有效速率（P15）',
      (tester) async {
    final dc = DeckController(0);
    final calls = <double>[];
    dc.syncOn.value = true;
    dc.rate.value = 3.0; // 滑杆位置 +3%
    dc.effRate.value = 1.5; // 引擎实际 +1.5%（sync 覆写）
    await tester.pumpWidget(_wrap(dc, onSetRate: calls.add));

    CustomPaint faderPaint() => tester.widget<CustomPaint>(
          find
              .descendant(
                  of: find.byType(TempoFader),
                  matching: find.byType(CustomPaint))
              .first,
        );

    final center = tester.getCenter(find.byType(TempoFader));
    await tester.dragFrom(center, const Offset(0, -70));
    expect(calls, isNotEmpty,
        reason: 'P15：sync 期间拖拽照常写 bus，生效与否由引擎软接管判定');

    // 微调 gate：sync 期间微调不写（防瞬移推子位置；暂时加减速用 nudge）
    final fineBefore = calls.length;
    final g = await tester.startGesture(tester.getCenter(find.text('−')));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    await tester.pump(const Duration(milliseconds: 100));
    expect(calls.length, fineBefore, reason: 'sync 期间微调不写 bus');
    await g.up();
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 300));

    // thumb 恒跟随有效速率（P14）：effRate 变化重绘，滑杆位置变化不驱动
    final p1 = faderPaint().painter;
    dc.effRate.value = 2.5;
    await tester.pump();
    final p2 = faderPaint().painter;
    expect(!identical(p1, p2), isTrue, reason: 'effRate 变化应重绘推子');
    dc.rate.value = 5.0;
    await tester.pump();
    final p3 = faderPaint().painter;
    expect(identical(p2, p3), isTrue, reason: '滑杆 bus 位置不驱动 thumb');

    // 关 sync（P14：推子仅解锁）：微调恢复；thumb 仍显实际速率
    //（不跳回滑杆位置——与引擎"解锁不改播放状态"一致）
    dc.syncOn.value = false;
    await tester.pump();
    final g2 = await tester.startGesture(tester.getCenter(find.text('−')));
    await tester.pump();
    await g2.up();
    await tester.pump();
    expect(calls.length, fineBefore + 1, reason: '关 sync 后微调恢复');
    await tester.pump(const Duration(milliseconds: 700));
  });

  testWidgets('双击推子回正 0%', (tester) async {
    final dc = DeckController(0);
    final calls = <double>[];
    await tester.pumpWidget(_wrap(dc, onSetRate: calls.add));
    // TempoFader 中心落在 rate 推子区（30..170），双击之
    final center = tester.getCenter(find.byType(TempoFader));
    await tester.tapAt(center);
    await tester.pump(const Duration(milliseconds: 60));
    await tester.tapAt(center);
    await tester.pump(const Duration(milliseconds: 700));
    expect(calls, [0.0]);
  });

  testWidgets('默认回调路径渲染（P18 推子列仅 −/+ 与推子；nudge 已移走）',
      (tester) async {
    final dc = DeckController(0);
    await tester.pumpWidget(_wrap(dc));
    for (final t in ['−', '+']) {
      expect(find.text(t), findsOneWidget);
    }
    expect(find.text('◀◀'), findsNothing);
    expect(find.text('▶▶'), findsNothing);
    expect(tester.takeException(), isNull);
  });
}
