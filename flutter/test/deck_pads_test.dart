// DeckPads / CueButton widget 测试：注入假 PadActions，脱离桥验证
// 按压语义、翻页窗口与删除交互。
//
// P22-D：loop pad 已改 onTapDown（按下即激活，同 beatjump P12 先例）——
// `tester.tap` 含按下+抬起，用例与断言均不变。

import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/deck_controller.dart';
import 'package:hypermixx/widgets/deck_pads.dart';

class FakeActions extends PadActions {
  final seeks = <(int, double)>[];
  final plays = <(int, bool)>[];
  final loopActiveSets = <(int, bool)>[];
  final beatLoops = <(int, double)>[];
  final jumps = <(int, double)>[];
  final fxEnables = <(int, int, bool)>[];

  @override
  void seekExactTo(int deck, double seconds) => seeks.add((deck, seconds));
  @override
  void setPlaying(int deck, bool on) => plays.add((deck, on));
  @override
  void setLoopActive(int deck, bool on) => loopActiveSets.add((deck, on));
  @override
  void activateBeatLoop(int deck, double beats) => beatLoops.add((deck, beats));
  @override
  void beatJump(int deck, double beats) => jumps.add((deck, beats));
  @override
  void setFxEnable(int deck, int slot, bool on) =>
      fxEnables.add((deck, slot, on));
}

Widget _wrap(Widget child) => MaterialApp(
      home: Scaffold(
        body: SizedBox(width: 720, height: 300, child: child),
      ),
    );

Finder _pad(DeckController dc, String label) =>
    find.descendant(of: find.byType(DeckPads), matching: find.text(label));

Finder _tab(String label) => find.descendant(
    of: find.byType(DeckPads), matching: find.text(label));

void main() {
  testWidgets('renders 4 mode tabs and 8 hotcue pads', (tester) async {
    final dc = DeckController(0);
    final fake = FakeActions();
    await tester.pumpWidget(_wrap(DeckPads(deck: dc, actions: fake)));
    await tester.pump();

    for (final t in ['HOTCUE', 'LOOP', 'BEATJUMP', 'FX']) {
      expect(_tab(t), findsOneWidget);
    }
    for (var i = 1; i <= 8; i++) {
      expect(_pad(dc, '$i'), findsOneWidget);
    }
  });

  testWidgets('hotcue: 停播远离落点 → 写入该槽，无跳转/播放', (tester) async {
    final dc = DeckController(0);
    final fake = FakeActions();
    dc.playhead.value = 5.0;
    await tester.pumpWidget(_wrap(DeckPads(deck: dc, actions: fake)));
    await tester.pump();

    await tester.tap(_pad(dc, '1'));
    await tester.pump();
    expect(dc.hotcues[0].value, 5.0);
    expect(fake.seeks, isEmpty);
    expect(fake.plays, isEmpty);
  });

  testWidgets('hotcue: 播放中 → 召回该槽点', (tester) async {
    final dc = DeckController(0);
    final fake = FakeActions();
    dc.playing.value = true;
    dc.hotcues[0].value = 2.5;
    await tester.pumpWidget(_wrap(DeckPads(deck: dc, actions: fake)));
    await tester.pump();

    await tester.tap(_pad(dc, '1'));
    await tester.pump();
    expect(fake.seeks, [(0, 2.5)]);
    expect(fake.plays, isEmpty);
  });

  testWidgets('hotcue: 位于点位按住试听，松开停播回点', (tester) async {
    final dc = DeckController(0);
    final fake = FakeActions();
    dc.hotcues[0].value = 2.0;
    dc.playhead.value = 2.0;
    await tester.pumpWidget(_wrap(DeckPads(deck: dc, actions: fake)));
    await tester.pump();

    final g = await tester.startGesture(tester.getCenter(_pad(dc, '1')));
    await tester.pump();
    expect(fake.plays, [(0, true)], reason: '按住应开始播放');
    await g.up();
    await tester.pump();
    expect(fake.plays, [(0, true), (0, false)], reason: '松开应停播');
    expect(fake.seeks, [(0, 2.0)], reason: '松开应回 cue 点');
  });

  testWidgets('hotcue: 右键与角上 × 都删除槽点', (tester) async {
    final dc = DeckController(0);
    final fake = FakeActions();
    dc.hotcues[0].value = 3.0;
    await tester.pumpWidget(_wrap(DeckPads(deck: dc, actions: fake)));
    await tester.pump();

    // 角上 ×
    await tester.tap(find.byIcon(Icons.close));
    await tester.pump();
    expect(dc.hotcues[0].value, isNull);

    // 右键（先恢复槽点）
    dc.hotcues[0].value = 4.0;
    await tester.pump();
    await tester.tap(_pad(dc, '1'), buttons: kSecondaryMouseButton);
    await tester.pump();
    expect(dc.hotcues[0].value, isNull);
  });

  testWidgets('loop: 点击激活，激活态再点取消', (tester) async {
    final dc = DeckController(0);
    final fake = FakeActions();
    dc.bpm.value = 120;
    await tester.pumpWidget(_wrap(DeckPads(deck: dc, actions: fake)));
    await tester.pump();

    await tester.tap(_tab('LOOP'));
    await tester.pump();
    await tester.tap(_pad(dc, '1')); // 1 拍 @120 = 0.5s
    await tester.pump();
    expect(fake.beatLoops, [(0, 1.0)]);

    // 模拟引擎回执：loop 激活 + in/out 匹配 1 拍
    dc.loopActive.value = true;
    dc.loopIn.value = 0.5;
    dc.loopOut.value = 1.0;
    await tester.pump();
    await tester.tap(_pad(dc, '1'));
    await tester.pump();
    expect(fake.loopActiveSets, [(0, false)], reason: '激活态再点应取消');
    expect(fake.beatLoops, [(0, 1.0)], reason: '不重复激活');
  });

  testWidgets('beatjump: P21 成对横排（◀1 ▶1 ◀2 ▶2 ...）+ 滚动间隔 4', (tester) async {
    final dc = DeckController(0);
    final fake = FakeActions();
    await tester.pumpWidget(_wrap(DeckPads(deck: dc, actions: fake)));
    await tester.pump();

    await tester.tap(_tab('BEATJUMP'));
    await tester.pump();
    // 页0：成对横排 ◀1 ▶1 ◀2 ▶2 / ◀4 ▶4 ◀8 ▶8
    expect(_pad(dc, '◀1'), findsOneWidget);
    expect(_pad(dc, '▶1'), findsOneWidget);
    expect(_pad(dc, '▶8'), findsOneWidget);
    expect(_pad(dc, '◀16'), findsNothing);
    // 左跳右跳相邻：◀1 在 ▶1 左侧，▶1 在 ◀2 左侧（横向排 = 成对相邻）
    final x1 = tester.getCenter(_pad(dc, '◀1')).dx;
    final x2 = tester.getCenter(_pad(dc, '▶1')).dx;
    final x3 = tester.getCenter(_pad(dc, '◀2')).dx;
    final x4 = tester.getCenter(_pad(dc, '▶2')).dx;
    expect(x1, lessThan(x2), reason: '◀1 在 ▶1 左');
    expect(x2, lessThan(x3), reason: '▶1 在 ◀2 左（成对相邻，非上下排）');
    expect(x3, lessThan(x4), reason: '◀2 在 ▶2 左');
    await tester.tap(_pad(dc, '▶1'));
    await tester.pump();
    expect(fake.jumps, [(0, 1.0)]);

    // 翻页（滚动间隔 4）：页1 = ◀4 ▶4 ◀8 ▶8 ◀16 ▶16 ◀32 ▶32
    await tester.tap(find.byIcon(Icons.keyboard_arrow_down));
    await tester.pump();
    expect(_pad(dc, '◀16'), findsOneWidget);
    expect(_pad(dc, '▶32'), findsOneWidget);
    expect(_pad(dc, '◀1'), findsNothing);
    expect(_pad(dc, '◀4'), findsOneWidget, reason: '滚动 4 项后 4 拍仍在页内');
    await tester.tap(_pad(dc, '◀32'));
    await tester.pump();
    expect(fake.jumps.last, (0, -32.0));
  });

  testWidgets('hotcue 翻页 3 页，边界钳制', (tester) async {
    final dc = DeckController(0);
    final fake = FakeActions();
    await tester.pumpWidget(_wrap(DeckPads(deck: dc, actions: fake)));
    await tester.pump();

    final down = find.byIcon(Icons.keyboard_arrow_down);
    await tester.tap(down);
    await tester.pump();
    expect(_pad(dc, '5'), findsOneWidget); // 窗口 5..12
    await tester.tap(down);
    await tester.pump();
    expect(_pad(dc, '9'), findsOneWidget);
    expect(_pad(dc, '16'), findsOneWidget); // 最后一页 9..16
    // 边界：下一页按钮禁用
    final btn = tester.widget<IconButton>(find.ancestor(
        of: down, matching: find.byType(IconButton)));
    expect(btn.onPressed, isNull);
  });

  testWidgets('fx: P18.1 只占位——8 pad 显示 FX、不可交互', (tester) async {
    final dc = DeckController(0);
    final fake = FakeActions();
    await tester.pumpWidget(_wrap(DeckPads(deck: dc, actions: fake)));
    await tester.pump();

    await tester.tap(_tab('FX'));
    await tester.pump();
    // 1 选项卡 + 8 占位 pad（功能并入 DeckFx 单通道）
    expect(_pad(dc, 'FX'), findsNWidgets(9));
    await tester.tap(find.text('FX').first);
    await tester.pump();
    await tester.tap(find.text('FX').first, buttons: kSecondaryMouseButton);
    await tester.pump();
    expect(fake.fxEnables, isEmpty, reason: '占位 pad 点击无动作');
  });

  testWidgets('CueButton: 播放暂停回点 / 停播落点 / 试听', (tester) async {
    final dc = DeckController(0);
    final fake = FakeActions();
    dc.cuePoint.value = 1.0;
    dc.playhead.value = 5.0;
    await tester.pumpWidget(_wrap(CueButton(deck: dc, actions: fake)));
    await tester.pump();

    // 停播远离 cue → 落点
    await tester.tap(find.byType(CueButton));
    await tester.pump();
    expect(dc.cuePoint.value, 5.0);
    expect(fake.seeks, isEmpty);

    // 停播位于 cue 点 → 按住试听、松开回点
    dc.playhead.value = 5.0;
    await tester.pump();
    final g = await tester.startGesture(tester.getCenter(find.byType(CueButton)));
    await tester.pump();
    expect(fake.plays, [(0, true)]);
    await g.up();
    await tester.pump();
    expect(fake.plays, [(0, true), (0, false)]);
    expect(fake.seeks, [(0, 5.0)]);

    // 播放中 → P19：暂停并回 cue 点（原：召回继续播）
    fake.plays.clear();
    fake.seeks.clear();
    dc.playing.value = true;
    dc.playhead.value = 7.0;
    await tester.pump();
    await tester.tap(find.byType(CueButton));
    await tester.pump();
    expect(fake.seeks, [(0, 5.0)]);
    expect(fake.plays, [(0, false)], reason: 'P19：播放中点击 CUE = 暂停并回 cue 点');
  });
}
