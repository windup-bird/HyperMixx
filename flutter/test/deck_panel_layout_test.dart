// 诊断：DeckPanel 行1 信息列的几何（时间/bpm/tempo/sync/key 是否同排居中）。

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/deck_controller.dart';
import 'package:hypermixx/widgets/deck_fx.dart';
import 'package:hypermixx/widgets/deck_pads.dart';
import 'package:hypermixx/widgets/deck_panel.dart';

void main() {
  testWidgets('row1 info column geometry', (tester) async {
    final dc = DeckController(0);
    dc.title = 'yehno - Always';
    dc.artist = 'artist';
    dc.timeText.value = '1:58 / 5:29';
    dc.bpmKeyText.value = '130.1 8A';
    dc.tempoText.value = '范围 ±8%\n当前 ±0.00%';

    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: SizedBox(
            width: 353,
            height: 460,
            child: DeckPanel(deck: dc),
          ),
        ),
      ),
    );
    await tester.pump();

    // P19：SYNC 移入 transport 行，信息列只查 KEY
    for (final t in ['1:58 / 5:29', '130.1 8A', '范围 ±8%\n当前 ±0.00%', 'KEY']) {
      final f = find.text(t);
      if (f.evaluate().isNotEmpty) {
        final rect = tester.getRect(f);
        debugPrint('text "$t": rect=$rect');
      } else {
        debugPrint('text "$t": NOT FOUND');
      }
    }
    debugPrint('panel size: ${tester.getSize(find.byType(DeckPanel))}');
  });

  testWidgets('P18.1 响应式：窄窗口（240px）面板不溢出', (tester) async {
    final dc = DeckController(0);
    dc.title = '窄窗测试曲目';
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: SizedBox(
            width: 240,
            height: 460,
            child: DeckPanel(deck: dc),
          ),
        ),
      ),
    );
    await tester.pump();
    // nudge Flexible 收缩 + pads/FittedBox 兜底：任何 RenderFlex overflow
    // 都会在测试里抛异常
    expect(tester.takeException(), isNull);
  });

  testWidgets('P21: DeckFx 填满右侧（fx 右缘与行尾对齐）', (tester) async {
    final dc = DeckController(0);
    dc.title = 'fx 平分测试';
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: SizedBox(
            width: 700,
            height: 460,
            child: DeckPanel(deck: dc),
          ),
        ),
      ),
    );
    await tester.pump();

    // DeckFx 与 DeckPads 的左右边界：pads 左侧 = 面板内容左缘；
    // fx 右缘应贴近面板右缘（不再留白）。+ 2px 间距。
    final fx = find.byType(DeckFx);
    final pads = find.byType(DeckPads);
    expect(fx, findsOneWidget);
    expect(pads, findsOneWidget);
    final fxRect = tester.getRect(fx);
    final padsRect = tester.getRect(pads);
    final panelRect = tester.getRect(find.byType(DeckPanel));
    // fx 左缘 > pads 右缘（fx 在 pads 右侧）
    expect(fxRect.left, greaterThan(padsRect.right));
    // fx 右缘顶到左列行尾：面板右缘 − fx 右缘 = tempo 列 72 + 间距 8 +
    // padding 10 + margin 6 = 96（fx 填满左侧列宽，不留白）
    expect(panelRect.right - fxRect.right, closeTo(96, 2),
        reason: 'fx 应填满左列行尾，实际余量 ${panelRect.right - fxRect.right}');
    // P22.3：pads:fx = 1:1 固定（两列宽相等，±2px 含 2px 间距偏差）
    expect(fxRect.width, closeTo(padsRect.width, 4),
        reason: 'pads/fx 应 1:1，实际 ${padsRect.width} vs ${fxRect.width}');
    expect(tester.takeException(), isNull);
  });

  testWidgets('P22.3: 中窗（400px）同样 1:1 且不溢出', (tester) async {
    final dc = DeckController(0);
    dc.title = '中窗 1:1 测试';
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: SizedBox(
            width: 400,
            height: 460,
            child: DeckPanel(deck: dc),
          ),
        ),
      ),
    );
    await tester.pump();

    final fx = find.byType(DeckFx);
    final pads = find.byType(DeckPads);
    expect(fx, findsOneWidget);
    expect(pads, findsOneWidget);
    final fxRect = tester.getRect(fx);
    final padsRect = tester.getRect(pads);
    expect(fxRect.width, closeTo(padsRect.width, 4),
        reason: '任何宽度下 pads/fx 都 1:1，实际 ${padsRect.width} vs ${fxRect.width}');
    expect(tester.takeException(), isNull);
  });
}
