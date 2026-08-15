// 诊断：OverviewWave 播放头 overlay 是否渲染、位置是否正确。

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/deck_controller.dart';
import 'package:hypermixx/widgets/overview_wave.dart';

void main() {
  testWidgets('overview playhead renders at expected x', (tester) async {
    final dc = DeckController(0);
    dc.duration.value = 329.0;
    dc.playhead.value = 60.0;

    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: SizedBox(
            width: 400,
            height: 64,
            child: OverviewWave(deck: dc),
          ),
        ),
      ),
    );
    await tester.pump();

    final containers = tester.widgetList<Container>(find.byType(Container));
    final white = containers.where((c) =>
        c.color == Colors.white.withValues(alpha: 0.9) || true).toList();
    for (final c in white) {
      // 播放头容器宽 2
      final size = tester.getSize(find.byWidget(c));
      final pos = tester.getTopLeft(find.byWidget(c));
      debugPrint('container: color=${c.color} size=$size pos=$pos');
    }
    final positioned = tester.widgetList<Positioned>(find.byType(Positioned));
    debugPrint('positioned count: ${positioned.length}');
    for (final p in positioned) {
      debugPrint('positioned: left=${p.left} top=${p.top} bottom=${p.bottom}');
    }
  });
}
