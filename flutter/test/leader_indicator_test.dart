//! P16 leader（master deck）指示 + P19 迁移：SYNC 从 deckinfo 列移入
//! transport 行（TransportRow），leader 轨 SYNC 按钮 amber 边框；
//! P22.4 用户要求 deckinfo 加回 SYNC（与 KEY 并列）——本文件所有 SYNC
//! 查找限定 transport 行（leader 指示只在此处）。
//! 判定规则与引擎一致：单开 = 不开 sync 的轨，双开 = deck0，都关无 leader。

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/engine_controller.dart';
import 'package:hypermixx/widgets/deck_panel.dart';
import 'package:hypermixx/widgets/transport_row.dart';

/// transport 行内的 sync 文本（master 状态显示为 MASTER）。
Finder _trSync(Finder panel) => find.descendant(
  of: find.descendant(of: panel, matching: find.byType(TransportRow)),
  matching: find.textContaining(RegExp(r'^(SYNC|MASTER)$')),
);

/// 泵入双面板（deck1 在左、deck0 在右），返回按面板序的 transport 行
/// SYNC 按钮容器。
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
            .ancestor(of: _trSync(panel), matching: find.byType(Container))
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
    engine.decks[0].syncMaster.value = false;
    engine.decks[1].syncMaster.value = false;
  });

  testWidgets('SYNC 在 transport 行 + P22.4 info 列加回；KEY 在 info 列', (
    tester,
  ) async {
    final dc = engine.decks[0];
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: SizedBox(width: 353, height: 460, child: DeckPanel(deck: dc)),
        ),
      ),
    );
    await tester.pump();

    // P22.4：每面板两个 SYNC——info 列（与 KEY 同排）+ transport 行（KEY 下方）
    final syncs = find.text('SYNC');
    expect(
      syncs,
      findsNWidgets(2),
      reason: 'P22.4：SYNC 加回 info 列（transport 行仍有）',
    );
    final keyRect = tester.getRect(find.text('KEY'));
    final syncTops =
        syncs
            .evaluate()
            .map((e) => tester.getTopLeft(find.byWidget(e.widget as Text)).dy)
            .toList()
          ..sort();
    expect(
      syncTops.first,
      closeTo(keyRect.top, 2),
      reason: 'info 列 SYNC 与 KEY 同排',
    );
    expect(
      syncTops.last,
      greaterThan(keyRect.bottom),
      reason: 'transport 行 SYNC 在 KEY 下方',
    );
    expect(
      tester.getSize(find.text('SYNC').at(1)).width,
      greaterThan(keyRect.width),
      reason: 'transport 按钮等分宽 > info 列小按钮',
    );
  });

  testWidgets('引擎 snapshot 标出的 master 显示 amber 边框', (tester) async {
    engine.decks[0].syncMaster.value = false;
    engine.decks[1].syncMaster.value = true;
    final [masterBtn, followerBtn] = await _pumpBoth(tester);

    expect(sideOf(masterBtn), isNotNull, reason: 'master 轨 SYNC 应有 amber 边框');
    expect(sideOf(masterBtn)?.color, const Color(0xFFFFB300));
    expect(sideOf(followerBtn), isNull);
  });

  testWidgets('master 转移时边框随引擎 snapshot 转移', (tester) async {
    engine.decks[0].syncMaster.value = true;
    engine.decks[1].syncMaster.value = false;
    final [deck1Btn, deck0Btn] = await _pumpBoth(tester);
    expect(sideOf(deck0Btn), isNotNull);
    expect(sideOf(deck1Btn), isNull);

    engine.decks[0].syncMaster.value = false;
    engine.decks[1].syncMaster.value = true;
    await tester.pump();
    final [d1, d0] = await _pumpBoth(tester);
    expect(sideOf(d1), isNotNull);
    expect(sideOf(d0), isNull);
  });
}
