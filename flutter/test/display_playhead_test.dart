// P13 displayPlayhead 外推单元测试：推进采样 → 按有效速率外推（滚动连续）；
// 停播/欠载冻结在最后采样点；seek 跳变重锚；无采样回退 playhead 真值。

import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/deck_controller.dart';

void main() {
  test('无采样：displayPlayhead 回退 playhead 真值（纯 widget 测试直设）', () {
    final dc = DeckController(0);
    dc.playhead.value = 42.0;
    expect(dc.displayPlayhead, 42.0);
  });

  test('播放推进：按有效速率外推（rate=1 时 ≈ 墙钟秒）', () async {
    final dc = DeckController(0);
    dc.updatePhSample(100, 1, 120, 120);
    final t0 = dc.displayPlayhead;
    expect(t0, closeTo(100.0, 1e-3), reason: '采样瞬间 ≈ 真值');
    await Future<void>.delayed(const Duration(milliseconds: 120));
    final t1 = dc.displayPlayhead;
    expect(t1, greaterThan(t0 + 0.08),
        reason: 'rate=1 外推：120ms 后应推进 ≈0.12s（t1=$t1）');
  });

  test('同步变速：外推速率 = bpm/gridBpm（音轨秒）', () async {
    final dc = DeckController(0);
    // grid 120、实际 123.84 → rate 1.032：显示按音轨秒推进
    dc.updatePhSample(50, 1, 123.84, 120);
    final t0 = dc.displayPlayhead;
    await Future<void>.delayed(const Duration(milliseconds: 120));
    final t1 = dc.displayPlayhead;
    expect(t1 - t0, closeTo(0.12 * 123.84 / 120, 0.02),
        reason: '外推速率应为 grid 有效速率（t0=$t0 t1=$t1）');
  });

  test('停播：采样冻结 → 显示钉在最后采样点', () {
    final dc = DeckController(0);
    dc.updatePhSample(100, 1, 120, 120);
    // 停播：playhead 连续采样不变 → 冻结，不越过引擎真值
    dc.updatePhSample(100, 0, 0, 120);
    expect(dc.displayPlayhead, 100.0, reason: '停播后显示 = 最后采样点');
  });

  test('欠载（播放中采样不变）：同样冻结', () {
    final dc = DeckController(0);
    dc.updatePhSample(100, 1, 120, 120);
    // 引擎欠载：playing 仍 1 但 playhead 不再推进 → 冻结
    dc.updatePhSample(100, 1, 120, 120);
    expect(dc.displayPlayhead, 100.0, reason: '欠载时显示 = 最后采样点');
  });

  test('seek 跳变：重锚不跨 seek 插值', () {
    final dc = DeckController(0);
    dc.updatePhSample(100, 0, 0, 0);
    dc.updatePhSample(200, 0, 0, 0);
    expect(dc.displayPlayhead, 200.0, reason: 'seek 后直接跟跳新位置');
  });
}
