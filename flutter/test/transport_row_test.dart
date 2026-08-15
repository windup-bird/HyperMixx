//! P19 transport 行测试：6 等分按钮——SHIFT 死键、SYNC 点击切换、
//! PLAY 播放/暂停切换 + 文字、<< >> 按住 nudge ±1 松开 0。
//! 动作经假 PadActions 记录（不碰桥）；CUE 细节见 deck_pads_test。

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/deck_controller.dart';
import 'package:hypermixx/engine/engine_controller.dart';
import 'package:hypermixx/widgets/deck_pads.dart';
import 'package:hypermixx/widgets/transport_row.dart';

class _FakeActions extends PadActions {
  final syncs = <bool>[];
  final plays = <bool>[];
  final nudges = <int>[];

  @override
  void setSync(int deck, bool on) => syncs.add(on);
  @override
  void setPlaying(int deck, bool on) => plays.add(on);
  @override
  void setNudge(int deck, int v) => nudges.add(v);
}

Widget _wrap(DeckController dc, PadActions a) {
  return MaterialApp(
    home: Scaffold(
      backgroundColor: const Color(0xFF1A1E24),
      body: Center(
        child: SizedBox(width: 353, height: 30, child: TransportRow(deck: dc, actions: a)),
      ),
    ),
  );
}

void main() {
  testWidgets('6 按钮等分：PLAY/CUE/SYNC/SHIFT/<< />> 各一（P20 顺序）', (tester) async {
    final dc = EngineController.instance.decks[0];
    addTearDown(() => dc.syncOn.value = false);
    final fake = _FakeActions();
    await tester.pumpWidget(_wrap(dc, fake));
    await tester.pump();

    for (final t in ['PLAY', 'CUE', 'SYNC', 'SHIFT', '<<', '>>']) {
      expect(find.text(t), findsOneWidget, reason: '按钮 $t 应存在');
    }
    // P20 顺序：PLAY 在 CUE 左、SYNC 在 SHIFT 左（x 递增）
    final rects = {
      for (final t in ['PLAY', 'CUE', 'SYNC', 'SHIFT'])
        t: tester.getCenter(find.text(t)).dx,
    };
    expect(rects['PLAY']!, lessThan(rects['CUE']!));
    expect(rects['CUE']!, lessThan(rects['SYNC']!));
    expect(rects['SYNC']!, lessThan(rects['SHIFT']!));
    // 6 按钮等分：每按钮宽 ≈ (353 − 5×4) / 6
    final w = tester.getSize(find.text('SYNC')).width;
    expect(w, greaterThan(40), reason: '等分按钮宽度应显著（≈55px）');
    expect(tester.takeException(), isNull);
  });

  testWidgets('SHIFT 死键：点击无动作', (tester) async {
    final dc = EngineController.instance.decks[0];
    final fake = _FakeActions();
    await tester.pumpWidget(_wrap(dc, fake));

    await tester.tap(find.text('SHIFT'));
    await tester.pump();
    expect(fake.syncs, isEmpty);
    expect(fake.plays, isEmpty);
    expect(fake.nudges, isEmpty);
  });

  testWidgets('SYNC：点击切换 setSync（关→开→关）', (tester) async {
    final dc = EngineController.instance.decks[0];
    addTearDown(() => dc.syncOn.value = false);
    final fake = _FakeActions();
    await tester.pumpWidget(_wrap(dc, fake));

    await tester.tap(find.text('SYNC'));
    await tester.pump();
    expect(fake.syncs, [true], reason: '点击开启 sync');
    dc.syncOn.value = true;
    await tester.pump();
    await tester.tap(find.text('SYNC'));
    await tester.pump();
    expect(fake.syncs, [true, false], reason: '再点关闭 sync');
  });

  testWidgets('PLAY：点击 setPlaying 切换 + 文字 PLAY/PAUSE（P20 英文化）', (tester) async {
    final dc = EngineController.instance.decks[0];
    addTearDown(() => dc.playing.value = false);
    final fake = _FakeActions();
    await tester.pumpWidget(_wrap(dc, fake));

    await tester.tap(find.text('PLAY'));
    await tester.pump();
    expect(fake.plays, [true]);
    dc.playing.value = true;
    await tester.pump();
    expect(find.text('PAUSE'), findsOneWidget, reason: '播放中显示 PAUSE');
    await tester.tap(find.text('PAUSE'));
    await tester.pump();
    expect(fake.plays, [true, false]);
    dc.playing.value = false;
    await tester.pump();
    expect(find.text('PLAY'), findsOneWidget, reason: '停播恢复显示 PLAY');
  });

  testWidgets('<< >> 按住 nudge ±1、松开 0（P17.1 互换）', (tester) async {
    final dc = EngineController.instance.decks[0];
    final fake = _FakeActions();
    await tester.pumpWidget(_wrap(dc, fake));

    final g = await tester.startGesture(tester.getCenter(find.text('<<')));
    await tester.pump();
    expect(fake.nudges, [1], reason: '<< 按住 = 加速 +1');
    await g.up();
    await tester.pump();
    expect(fake.nudges, [1, 0], reason: '松开回 0');

    fake.nudges.clear();
    final g2 = await tester.startGesture(tester.getCenter(find.text('>>')));
    await tester.pump();
    expect(fake.nudges, [-1], reason: '>> 按住 = 减速 −1');
    await g2.up();
    await tester.pump();
    expect(fake.nudges, [-1, 0]);
  });

  testWidgets('窄窗（160px）6 按钮不溢出', (tester) async {
    final dc = EngineController.instance.decks[0];
    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: Center(
          child: SizedBox(
            width: 160,
            height: 30,
            child: TransportRow(deck: dc),
          ),
        ),
      ),
    ));
    await tester.pump();
    expect(tester.takeException(), isNull);
  });
}
