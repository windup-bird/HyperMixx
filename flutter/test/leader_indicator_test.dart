//! P16 leader（master deck）指示 + P19 迁移：SYNC 从 deckinfo 列移入
//! transport 行（TransportRow），leader 轨 SYNC 按钮 amber 边框；
//! 判定规则与引擎一致：单开 = 不开 sync 的轨，双开 = deck0，都关无 leader。

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/engine_controller.dart';
import 'package:hypermixx/widgets/deck_panel.dart';

/// 泵入双面板（deck1 在左、deck0 在右），返回按面板序的 SYNC 按钮容器。
Future<List<Container>> _pumpBoth(WidgetTester tester) async {
  final engine = EngineController.instance;
  await tester.pumpWidget(
    MaterialApp(
      home: Scaffold(
        body: Row(
          children: [
            SizedBox(
              width: 353,
              height: 460,
              child: DeckPanel(deck: engine.decks[1]),
            ),
            SizedBox(
              width: 353,
              height: 460,
              child: DeckPanel(deck: engine.decks[0]),
            ),
          ],
        ),
      ),
    ),
  );
  await tester.pump();
  final btns = <Container>[];
  for (var i = 0; i < 2; i++) {
    final panel = find.byType(DeckPanel).at(i);
    btns.add(
      tester.widget<Container>(
        find
            .ancestor(
              of: find.descendant(of: panel, matching: find.text('SYNC')),
              matching: find.byType(Container),
            )
            .first,
      ),
    );
  }
  return btns;
}

BorderSide? sideOf(Container c) {
  final deco = c.decoration;
  final b = deco is BoxDecoration ? deco.border : null;
  return b is Border ? b.top : null;
}

void main() {
  final engine = EngineController.instance;

  tearDown(() {
    engine.decks[0].syncOn.value = false;
    engine.decks[1].syncOn.value = false;
  });

  testWidgets('SYNC 在 transport 行（6 等分按钮之一），KEY 在 info 列', (tester) async {
    final dc = engine.decks[0];
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: SizedBox(width: 353, height: 460, child: DeckPanel(deck: dc)),
        ),
      ),
    );
    await tester.pump();

    final syncRect = tester.getRect(find.text('SYNC'));
    final keyRect = tester.getRect(find.text('KEY'));
    // transport 行在 info 列下方：SYNC 的 y 明显大于 KEY 的 y
    expect(syncRect.top, greaterThan(keyRect.bottom),
        reason: 'SYNC 移入 transport 行（KEY 下方）');
    expect(syncRect.width, greaterThan(keyRect.width),
        reason: 'transport 按钮等分宽 > info 列小按钮');
  });

  testWidgets('单开 sync：未开 sync 轨 = leader（amber 边框），开 sync 轨无', (tester) async {
    engine.decks[0].syncOn.value = true; // deck0 跟随 → deck1 是 leader
    engine.decks[1].syncOn.value = false;
    final [leaderBtn, followerBtn] = await _pumpBoth(tester);

    expect(sideOf(leaderBtn), isNotNull, reason: 'leader 轨 SYNC 应有 amber 边框');
    expect(sideOf(leaderBtn)?.color, const Color(0xFFFFB300));
    expect(sideOf(followerBtn), isNull, reason: 'follower 轨 SYNC 无边框');
  });

  testWidgets('双开 sync：deck0 = leader；都关：无 leader', (tester) async {
    engine.decks[0].syncOn.value = true;
    engine.decks[1].syncOn.value = true;
    final [deck1Btn, deck0Btn] = await _pumpBoth(tester);
    expect(sideOf(deck0Btn), isNotNull, reason: '双开时 deck0 是 leader');
    expect(sideOf(deck1Btn), isNull, reason: 'deck1 是 follower');

    engine.decks[0].syncOn.value = false;
    engine.decks[1].syncOn.value = false;
    await tester.pump();
    final [d1, d0] = await _pumpBoth(tester);
    expect(sideOf(d0), isNull, reason: '都关 sync：无 leader 指示');
    expect(sideOf(d1), isNull);
  });

  testWidgets('leader 轨开启 sync 后指示消失（现在另一轨是 leader）', (tester) async {
    engine.decks[0].syncOn.value = true;
    engine.decks[1].syncOn.value = false;
    final [leaderBtn, _] = await _pumpBoth(tester);
    expect(sideOf(leaderBtn), isNotNull);

    // 模拟引擎回调：deck1（原 leader）开 sync → 双开 → deck0 成为 leader
    engine.decks[0].syncOn.value = true;
    engine.decks[1].syncOn.value = true;
    await tester.pump();
    final [d1, d0] = await _pumpBoth(tester);
    expect(sideOf(d1), isNull, reason: '双开后 deck1 不再是 leader');
    expect(sideOf(d0), isNotNull, reason: '双开后 deck0 是 leader');
  });
}
