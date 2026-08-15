// CUE/hotcue 状态机纯函数单测（无桥、无 widget）。

import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/engine/cue_action.dart';

void main() {
  test('playing → recall 到已有点，松开无动作', () {
    final r = nextCueAction(playing: true, playhead: 5.0, point: 1.2);
    expect(r.kind, CuePressKind.recall);
    expect(r.point, 1.2);
    expect(cueReleaseSeek(r), isNull);
  });

  test('停播且远离 cue → 落点为当前播头，不跳转', () {
    final r = nextCueAction(playing: false, playhead: 5.0, point: 1.2);
    expect(r.kind, CuePressKind.set);
    expect(r.cueToSet, 5.0);
    expect(cueReleaseSeek(r), isNull);
  });

  test('停播且位于 cue 点 → 试听，松开停播回点', () {
    final r = nextCueAction(playing: false, playhead: 1.2, point: 1.2);
    expect(r.kind, CuePressKind.preview);
    expect(r.point, 1.2);
    expect(cueReleaseSeek(r), 1.2);
  });

  test('epsilon 容差内判为位于 cue 点', () {
    final near = nextCueAction(
        playing: false, playhead: 1.2 + kCueEpsilonSecs * 0.5, point: 1.2);
    expect(near.kind, CuePressKind.preview);
    final far = nextCueAction(
        playing: false, playhead: 1.2 + kCueEpsilonSecs * 2, point: 1.2);
    expect(far.kind, CuePressKind.set);
    expect(far.cueToSet, 1.2 + kCueEpsilonSecs * 2);
  });

  test('空点（空 hotcue）→ 落点，无论是否播放', () {
    final paused = nextCueAction(playing: false, playhead: 3.3, point: null);
    expect(paused.kind, CuePressKind.set);
    expect(paused.cueToSet, 3.3);
    final playing = nextCueAction(playing: true, playhead: 3.3, point: null);
    expect(playing.kind, CuePressKind.set);
    expect(playing.cueToSet, 3.3);
  });
}
