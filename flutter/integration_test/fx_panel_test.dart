// P18.1/P20 DeckFx 交互：真桥 + 真引擎。
// 用 integration_test（Linux 桌面可跑）：真实 app + 真实 engine + 真实控制总线。
//
// 覆盖：▶/◀ 切型写 manifest 默认值；旋钮下方开关启用/停用 enable、名称
// 按钮左键单击选型菜单（P20 右键事件改左键）；门控节拍 ×2/÷2 吸附写 p1；
// 强度旋钮拖拽 drywet（0 值 −150° 线性扫角）；FX pad 只占位（点击不写
// enable）。
//
// 注意：60Hz tick 持续调度帧 → 禁用 pumpAndSettle，全用显式 pump。
// 窗口拉到 1600×1800 逻辑像素，保证两 deck 面板完整可见。

import 'package:flutter/gestures.dart' show kSecondaryMouseButton;
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

import 'package:hypermixx/main.dart' as app;
import 'package:hypermixx/src/rust/api.dart';
import 'package:hypermixx/widgets/deck_fx.dart';
import 'package:hypermixx/widgets/deck_pads.dart';
import 'package:hypermixx/widgets/mixer_knob.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('DeckFx: 切型写默认值、中间激活/菜单、门控节拍、旋钮、FX pad 占位',
      (tester) async {
    tester.view.physicalSize = const Size(1600, 1800);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.reset);

    await app.main(); // 载桥 + 启动引擎
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));

    final d1 = find.byType(DeckFx).first;
    Finder inFx(String text) =>
        find.descendant(of: d1, matching: find.text(text));

    // 初始：type 0 / enable 0 / 名称 'FX'（未选型不亮）/ 节拍 '–'
    expect(busGet(path: 'Deck1.fx1_type'), 0);
    expect(busGet(path: 'Deck1.fx1_enable'), 0);
    expect(inFx('FX'), findsOneWidget);
    expect(inFx('–'), findsOneWidget);

    // ---- ▶ 下一个：0 → 1（回声）+ manifest 默认值写 p1..p4 ----
    await tester.tap(inFx('▶'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    expect(busGet(path: 'Deck1.fx1_type'), moreOrLessEquals(1, epsilon: 1e-6));
    expect(busGet(path: 'Deck1.fx1_p1'), moreOrLessEquals(0.375, epsilon: 1e-3));
    expect(busGet(path: 'Deck1.fx1_p2'), moreOrLessEquals(0.35, epsilon: 1e-3));
    expect(inFx('回声'), findsOneWidget, reason: '名称上屏 = manifest label');

    // ---- 旋钮下方开关（P20）：enable 1 → 0 ----
    final fxSwitch = find.descendant(
      of: find.byType(DeckFx).first,
      matching: find.byType(Switch),
    );
    await tester.tap(fxSwitch);
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));
    expect(busGet(path: 'Deck1.fx1_enable'), 1, reason: '开关激活');
    await tester.tap(fxSwitch);
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));
    expect(busGet(path: 'Deck1.fx1_enable'), 0, reason: '再点取消');

    // ---- ▶ 连续点击循环到 8（门控）----
    for (var i = 1; i <= 7; i++) {
      await tester.tap(inFx('▶'));
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 50));
    }
    expect(busGet(path: 'Deck1.fx1_type'), moreOrLessEquals(8, epsilon: 1e-6));
    expect(inFx('门控'), findsOneWidget);
    // 门控唯一拍参数 gate.period（p1）默认 1.0 → 节拍行 '1'，÷2/×2 可用
    expect(busGet(path: 'Deck1.fx1_p1'), moreOrLessEquals(1.0, epsilon: 1e-3));
    expect(inFx('1'), findsOneWidget);

    // ---- 节拍 ×2/÷2：吸附写 p1（1→2→4→2）----
    await tester.tap(inFx('×2'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));
    expect(busGet(path: 'Deck1.fx1_p1'), moreOrLessEquals(2.0, epsilon: 1e-3));
    expect(inFx('2'), findsOneWidget);

    await tester.tap(inFx('×2'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));
    expect(busGet(path: 'Deck1.fx1_p1'), moreOrLessEquals(4.0, epsilon: 1e-3));

    await tester.tap(inFx('÷2'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));
    expect(busGet(path: 'Deck1.fx1_p1'), moreOrLessEquals(2.0, epsilon: 1e-3));

    // ---- 中间拍数按钮：点击回 manifest 默认（2 → 1）----
    await tester.tap(inFx('2'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));
    expect(busGet(path: 'Deck1.fx1_p1'), moreOrLessEquals(1.0, epsilon: 1e-3));

    // ---- 强度旋钮：右拖加值 → drywet 总线 > 0 ----
    // 从旋钮本体（MixerKnob 内唯一 GestureDetector = 旋钮盘）起拖——
    // 标签文字在旋钮下方，从文字起拖会脱靶。
    final knob = find.descendant(
      of: find
          .descendant(of: d1, matching: find.byType(MixerKnob))
          .first,
      matching: find.byType(GestureDetector),
    );
    await tester.dragFrom(tester.getCenter(knob), const Offset(60, 0));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));
    final drywet = busGet(path: 'Deck1.fx1_drywet');
    // dragFrom 吃 ~20px 手势 slop：60px 实际 ~40px → ~0.29（线性 300°/140px）
    expect(drywet, greaterThan(0.2), reason: '右拖加 drywet');
    expect(drywet, lessThanOrEqualTo(1.0));

    // ---- 名称按钮左键单击：选型菜单（门控 → 回声，P20 右键事件改左键）----
    await tester.tap(inFx('门控'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 400)); // sheet 动画
    final sheet = find.byType(BottomSheet);
    expect(
      find.descendant(of: sheet, matching: find.text('无效果')),
      findsOneWidget,
    );
    // 菜单里选 回声：type 1 + manifest 默认值重写
    await tester.tap(find.descendant(of: sheet, matching: find.text('回声')));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 200));
    expect(busGet(path: 'Deck1.fx1_type'), moreOrLessEquals(1, epsilon: 1e-6));
    expect(busGet(path: 'Deck1.fx1_p1'), moreOrLessEquals(0.375, epsilon: 1e-3));

    // ---- FX pad 只占位：点击/右键均不写总线 ----
    await tester.tap(find
        .descendant(of: find.byType(DeckPads).first, matching: find.text('FX')));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));
    final enableBefore = busGet(path: 'Deck1.fx1_enable');
    final pads = find
        .descendant(of: find.byType(DeckPads).first, matching: find.text('FX'));
    expect(pads, findsNWidgets(9), reason: '1 选项卡 + 8 占位 pad');
    await tester.tap(pads.first);
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));
    await tester.tap(pads.first, buttons: kSecondaryMouseButton);
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    expect(busGet(path: 'Deck1.fx1_enable'), enableBefore,
        reason: '占位 pad 点击/右键不写 enable');
  });
}
