// 冒烟测试：桥缺失路径渲染提示屏（测试环境无桥库，走 bridgeMissing 分支）。
// 完整主屏在真实运行中验证（HYPERMIXX_BRIDGE_LIB + flutter run）。

import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/main.dart';

void main() {
  testWidgets('bridge missing shows hint screen', (WidgetTester tester) async {
    await tester.pumpWidget(const HyperMixxApp(bridgeMissing: true));
    await tester.pump();

    expect(find.textContaining('libhypermixx_bridge.so'), findsOneWidget);
  });
}
