// FX 面板：manifest 驱动的 8 槽架渲染测试。
// 桥缺失（测试环境）：fxManifestsCache 直接注入假清单，槽 initState 的
// busGet 走防御分支返回 0 → 显示"无效果"。

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/deck_controller.dart';
import 'package:hypermixx/engine/engine_controller.dart';
import 'package:hypermixx/src/rust/api.dart';
import 'package:hypermixx/widgets/fx_panel.dart';

FxParamWire _p(String name, String label, String unit, double min, double max,
    {double step = 0, bool stepped = false, double dflt = 0}) {
  return FxParamWire(
    name: name,
    label: label,
    unit: unit,
    kindStepped: stepped,
    kindMin: min,
    kindMax: max,
    kindStep: step,
    defaultValue: dflt,
  );
}

void main() {
  testWidgets('renders 8 slots with effect list from manifest', (tester) async {
    final engine = EngineController.instance;
    engine.fxManifestsCache = [
      FxEffectWire(id: 1, name: 'echo', label: '回声', params: [
        _p('time', 'Time', 's', 0.01, 2.0, dflt: 0.375),
        _p('sync', 'Sync', '', 0, 1, step: 1, stepped: true),
      ]),
      FxEffectWire(id: 2, name: 'reverb', label: '混响', params: [
        _p('roomsize', 'Room Size', 'ratio', 0, 1),
      ]),
    ];

    final dc = DeckController(0);
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          // 8 槽在 600px 视口放不下：像真实 show() 一样套滚动容器
          body: SingleChildScrollView(child: FxPanel(deck: dc)),
        ),
      ),
    );
    await tester.pump();

    expect(find.text('FX RACK'), findsOneWidget);
    // 8 槽
    for (var s = 1; s <= 8; s++) {
      expect(find.text('FX$s'), findsOneWidget);
    }
    // 无选中效果 → 每个槽显示"无效果" hint（关闭时下拉项不构建，只 hint）
    expect(find.text('无效果'), findsNWidgets(8));

    // 打开槽1下拉：manifest 效果标签出现在菜单项
    await tester.tap(find.byType(DropdownButton<int>).first);
    await tester.pumpAndSettle();
    expect(find.text('回声'), findsOneWidget);
    expect(find.text('混响'), findsOneWidget);
  });

  testWidgets('empty manifests shows not-connected fallback', (tester) async {
    EngineController.instance.fxManifestsCache = const [];
    final dc = DeckController(0);
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(body: FxPanel(deck: dc)),
      ),
    );
    await tester.pump();
    expect(find.text('引擎未连接（无效果清单）'), findsOneWidget);
  });
}
