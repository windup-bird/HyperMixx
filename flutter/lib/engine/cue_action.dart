//! CUE / hotcue 按下行为决策（纯函数：无桥依赖，widget 测试直接断言）。
//!
//! 用户规格（P19 起主 CUE）：
//! - 播放时点击 → 回到 cue 点：主 CUE 按钮 = 暂停并回点（调用方见
//!   deck_pads.dart CueButton）；hotcue = 召回继续播；
//! - 不播放（或拖拽波形后）→ 把当前指针位置设为 cue 点；
//! - 不播放且指针位于 cue 点 → 按下开始播放，释放回到 cue 点（试听）。
//! hotcue 传入自身存储点（null = 空槽 → 按下即落点），决策逻辑相同。

/// 判"指针位于 cue 点"的容差（秒）。引擎 seek 落点精确到帧，
/// 容差只兜住播头冻结后 ±1 块（5.3ms）的抖动。
const double kCueEpsilonSecs = 0.05;

/// 按下动作类别。
enum CuePressKind {
  /// 播放中：召回（跳回 cue 点；主 CUE 调用方同时暂停，hotcue 继续播）。
  recall,

  /// 未播放且不在 cue 点：把当前播头设为 cue 点（不跳转）。
  set,

  /// 未播放且正位于 cue 点：按住试听（播），松开回 cue 点。
  preview,
}

/// 按下决策结果。字段按 kind 取值：
/// - recall / preview：`point` = 已有 cue 点；
/// - set：`cueToSet` = 新 cue 点（= 当前播头）。
class CuePressResult {
  const CuePressResult(this.kind, {this.point, this.cueToSet})
      : assert(kind != CuePressKind.set || cueToSet != null),
        assert(kind == CuePressKind.set || point != null);

  final CuePressKind kind;
  final double? point;
  final double? cueToSet;
}

/// CUE / hotcue 按下行为决策。`point`：主 cue 传 cuePoint，
/// hotcue 传该槽存储点（空槽 null → 落点）。
CuePressResult nextCueAction({
  required bool playing,
  required double playhead,
  required double? point,
}) {
  if (point == null) {
    // 空 hotcue：按下即把当前播头落为该点
    return CuePressResult(CuePressKind.set, cueToSet: playhead);
  }
  if (playing) {
    return CuePressResult(CuePressKind.recall, point: point);
  }
  if ((playhead - point).abs() <= kCueEpsilonSecs) {
    return CuePressResult(CuePressKind.preview, point: point);
  }
  return CuePressResult(CuePressKind.set, cueToSet: playhead);
}

/// 松开 CUE 后的动作：仅 preview 需要（停播并 seekExact 回 cue 点）。
/// 返回 null = 松开无动作。非 preview 的按下是瞬时动作，松开忽略。
double? cueReleaseSeek(CuePressResult press) =>
    press.kind == CuePressKind.preview ? press.point : null;
