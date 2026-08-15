//! DeckFx 测试（P18.1）：纯函数（拍参数查找 / 类型循环 / 步进吸附 /
//! 线性扫角）+ widget（◀▶ 选型写默认值、中间左键激活、右键选型菜单、
//! 拍数 ×2/÷2/回默认、无拍参数时 '–' 禁用）。
//! 桥缺失 → busGet 回 0：类型恒 0，状态写入经假 PadActions 记录。

import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/deck_controller.dart';
import 'package:hypermixx/engine/engine_controller.dart';
import 'package:hypermixx/src/rust/api.dart';
import 'package:hypermixx/widgets/deck_fx.dart';
import 'package:hypermixx/widgets/deck_pads.dart';
import 'package:hypermixx/widgets/mixer_knob.dart';
import 'package:hypermixx/widgets/panel_button.dart';

/// 记录调用的假动作出口。
class _FakeActions extends PadActions {
  final fxTypes = <int>[];
  final fxEnable = <bool>[];
  final params = <(int, double)>[];

  @override
  void setFxType(int deck, int slot, int fxId) => fxTypes.add(fxId);
  @override
  void setFxEnable(int deck, int slot, bool on) => fxEnable.add(on);
  @override
  void setFxParam(int deck, int slot, int paramIdx, double v) =>
      params.add((paramIdx, v));
}

// 假 manifest：id1 回声（无拍参数）、id8 门控（gate.period = beats）
final _echoTime = FxParamWire(
  name: 'time',
  label: 'Time',
  unit: 's',
  kindStepped: false,
  kindMin: 0.01,
  kindMax: 2.0,
  kindStep: 0,
  defaultValue: 0.375,
);
final _echoFb = FxParamWire(
  name: 'feedback',
  label: 'Feedback',
  unit: 'ratio',
  kindStepped: false,
  kindMin: 0.0,
  kindMax: 0.95,
  kindStep: 0,
  defaultValue: 0.35,
);
final _gatePeriod = FxParamWire(
  name: 'period',
  label: 'Period',
  unit: 'beats',
  kindStepped: true,
  kindMin: 0.25,
  kindMax: 8.0,
  kindStep: 0.25,
  defaultValue: 1.0,
);
final _gate = FxEffectWire(id: 8, name: 'gate', label: '门控', params: [_gatePeriod]);
final _echo = FxEffectWire(id: 1, name: 'echo', label: '回声', params: [_echoTime, _echoFb]);

Widget _wrap(DeckController dc, _FakeActions a) {
  return MaterialApp(
    home: Scaffold(
      backgroundColor: const Color(0xFF1A1E24),
      body: Center(
        child: SizedBox(
          width: 220,
          height: 120, // P20 放大：旋钮 44 + 开关 + 两行 28
          child: DeckFx(deck: dc, actions: a),
        ),
      ),
    ),
  );
}

void main() {
  test('fxBeatsParam：仅 unit==beats 参数命中（门控 period），其余 null', () {
    final manifests = [_echo, _gate];
    final g = fxBeatsParam(manifests, 8);
    expect(g, isNotNull);
    expect(g!.$1, 0, reason: 'period 是 gate 的 p1');
    expect(g.$2.unit, 'beats');
    expect(fxBeatsParam(manifests, 1), isNull, reason: '回声无拍参数');
    expect(fxBeatsParam(manifests, 0), isNull);
    expect(fxBeatsParam(manifests, 9), isNull, reason: '未知类型');
  });

  test('nextFxType：0→1..count→0 循环', () {
    expect(nextFxType(0, 8), 1);
    expect(nextFxType(5, 8), 6);
    expect(nextFxType(8, 8), 0);
    expect(nextFxType(0, 0), 0, reason: '无效果清单：点击保持无效果');
  });

  test('prevFxType：1→0→count→count−1 循环（P18.1 ◀ 按钮）', () {
    expect(prevFxType(1, 8), 0);
    expect(prevFxType(0, 8), 8);
    expect(prevFxType(3, 8), 2);
    expect(prevFxType(0, 0), 0, reason: '无效果清单：点击保持无效果');
  });

  test('fxBeatSnap：吸附到 0.25 步进 + clamp', () {
    expect(fxBeatSnap(1.0 * 2, _gatePeriod), 2.0);
    expect(fxBeatSnap(1.0 / 2, _gatePeriod), 0.5);
    expect(fxBeatSnap(0.6 * 2, _gatePeriod), 1.25, reason: '1.2 吸附到 1.25');
    expect(fxBeatSnap(8.0 * 2, _gatePeriod), 8.0, reason: '上限 clamp');
  });

  test('knobAngleLinear/knobValueLinear：0 值 −150°、max +150° 线性 300°', () {
    // 0 → −150°（−5π/6），1.0 → +150°（+5π/6），0.5 → 0°
    expect(knobAngleLinear(0, 0, 1, -150), closeTo(-5 * math.pi / 6, 1e-9));
    expect(knobAngleLinear(1, 0, 1, -150), closeTo(5 * math.pi / 6, 1e-9));
    expect(knobAngleLinear(0.5, 0, 1, -150), closeTo(0, 1e-9));
    // 逆映射
    expect(knobValueLinear(-5 * math.pi / 6, 0, 1, -150), closeTo(0, 1e-9));
    expect(knobValueLinear(5 * math.pi / 6, 0, 1, -150), closeTo(1, 1e-9));
    expect(knobValueLinear(0, 0, 1, -150), closeTo(0.5, 1e-9));
    // 越界 clamp
    expect(knobValueLinear(10.0, 0, 1, -150), 1.0);
  });

  testWidgets('初始：type 0 显示 FX、–、开关在旋钮下、无拍参数 ÷2/×2 禁用', (tester) async {
    EngineController.instance.fxManifestsCache = [_echo, _gate];
    addTearDown(() => EngineController.instance.fxManifestsCache = const []);
    final dc = DeckController(0);
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    expect(find.text('FX'), findsOneWidget, reason: 'type 0 名称');
    expect(find.text('–'), findsOneWidget, reason: 'type 0 无拍参数 → 禁用');
    expect(find.text('强度'), findsNothing, reason: 'P22.1：强度 label 已删');
    expect(find.text('◀'), findsOneWidget);
    expect(find.text('▶'), findsOneWidget);
    expect(find.byType(Switch), findsOneWidget, reason: 'P20：旋钮下方 on/off 开关');
    // 开关在旋钮下方
    final knobY = tester.getCenter(find.byType(MixerKnob)).dy;
    final swY = tester.getCenter(find.byType(Switch)).dy;
    expect(swY, greaterThan(knobY), reason: '开关应在旋钮下方');
    // P22.1：上下边界严格对齐——行1 顶 = 旋钮顶、行2 底 = 开关底；
    // 行1/行2 紧贴（中心间距 = 行高 42 + 6）。按钮用 PanelButton 定位
    // （文字在按钮内居中，不能用 find.text 的位置代表按钮位置）。
    final knobTop = tester.getTopLeft(find.byType(MixerKnob)).dy;
    final row1Top =
        tester.getTopLeft(find.widgetWithText(PanelButton, '÷2')).dy;
    expect((row1Top - knobTop).abs(), lessThan(1), reason: '行1 顶与旋钮顶对齐');
    final swBottom = tester.getBottomRight(find.byType(Switch)).dy;
    final row2Bottom =
        tester.getBottomRight(find.widgetWithText(PanelButton, '◀')).dy;
    expect((row2Bottom - swBottom).abs(), lessThan(1), reason: '行2 底与开关底对齐');
    final row1Y = tester.getCenter(find.widgetWithText(PanelButton, '÷2')).dy;
    final row2Y = tester.getCenter(find.widgetWithText(PanelButton, '◀')).dy;
    expect(row2Y - row1Y, closeTo(kFxRowHeight + 6, 1), reason: '行1/行2 紧贴');
    // P22.3：beat（行1）与 select（行2）按钮横向铺满 fx 列——行尾按钮
    // ×2 / ▶ 的右缘都贴近 DeckFx 右缘（Expanded 填满，不留白）
    final fxRight = tester.getRect(find.byType(DeckFx)).right;
    final x2Right = tester.getRect(find.widgetWithText(PanelButton, '×2')).right;
    final selRight =
        tester.getRect(find.widgetWithText(PanelButton, '▶')).right;
    expect(fxRight - x2Right, lessThan(1), reason: '行1（beat）按钮应铺满 fx 列宽');
    expect(fxRight - selRight, lessThan(1), reason: '行2（select）按钮应铺满 fx 列宽');
    expect(tester.takeException(), isNull);
  });

  testWidgets('开关：点击写 enable（P20 启用由开关负责）', (tester) async {
    EngineController.instance.fxManifestsCache = [_echo, _gate];
    addTearDown(() => EngineController.instance.fxManifestsCache = const []);
    final dc = DeckController(0);
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    await tester.tap(find.byType(Switch));
    await tester.pump();
    expect(a.fxEnable, [true], reason: '开关点击写 enable true');
    // 再点关闭：widget 测试无桥 → 总线恒 0 → Switch value 恒 false →
    // 每次点击都传 !value = true；真实 toggle 回写由 integration 验证。
  });

  testWidgets('▶ 下一个：写 type + manifest 默认值到 p1..p4', (tester) async {
    EngineController.instance.fxManifestsCache = [_echo, _gate];
    addTearDown(() => EngineController.instance.fxManifestsCache = const []);
    final dc = DeckController(0);
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    await tester.tap(find.text('▶'));
    await tester.pump();
    expect(a.fxTypes, [1], reason: '0 → 下一效果（回声）');
    expect(a.params, [
      (0, 0.375),
      (1, 0.35),
    ], reason: '选型写 manifest 默认值到 p1..p2');
  });

  testWidgets('◀ 上一个：0 → count（循环回末尾）并写默认值', (tester) async {
    EngineController.instance.fxManifestsCache = [_echo, _gate];
    addTearDown(() => EngineController.instance.fxManifestsCache = const []);
    final dc = DeckController(0);
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    await tester.tap(find.text('◀'));
    await tester.pump();
    // 测试 manifest 只有 id 1/8（效果个数 2）：prev(0) = 2，但 id 2
    // 无 manifest → 仅写 type 不写默认值（防御分支；真实引擎 id 连续 1..8）
    expect(a.fxTypes, [2], reason: 'prevFxType(0, 2) = 2');
    expect(a.params, isEmpty, reason: 'id 2 不在假 manifest 里');
  });

  testWidgets('名称按钮左键单击：选型菜单列全部效果，选型写默认值（P20 右键事件改左键）',
      (tester) async {
    EngineController.instance.fxManifestsCache = [_echo, _gate];
    addTearDown(() => EngineController.instance.fxManifestsCache = const []);
    final dc = DeckController(0);
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    await tester.tap(find.text('FX'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 300)); // sheet 动画
    expect(find.text('无效果'), findsOneWidget);
    expect(find.text('回声'), findsOneWidget);
    expect(find.text('门控'), findsOneWidget);
    expect(a.fxEnable, isEmpty, reason: '菜单本身不写 enable');

    await tester.tap(find.text('回声'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 300));
    expect(a.fxTypes, [1]);
    expect(a.params, [(0, 0.375), (1, 0.35)]);
  });

  testWidgets('菜单选无效果：type 0 + 关 enable', (tester) async {
    EngineController.instance.fxManifestsCache = [_echo, _gate];
    addTearDown(() => EngineController.instance.fxManifestsCache = const []);
    final dc = DeckController(0);
    final a = _FakeActions();
    await tester.pumpWidget(_wrap(dc, a));

    await tester.tap(find.text('FX'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 300));
    await tester.tap(find.text('无效果'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 300));
    expect(a.fxTypes, [0]);
    expect(a.fxEnable, [false], reason: '选无效果关 enable');
  });
}
