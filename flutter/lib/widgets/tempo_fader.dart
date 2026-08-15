//! Tempofader 列：垂直变速推子 ±8%（50% 处 0 刻度线）+ 微调按住重复键。
//!
//! P19：nudge 键已移到 transport 行（TransportRow，<< 加速 / >> 减速），
//! 推子列只剩变速 + 微调。微调用 Timer.periodic(100ms) 每拍 +0.5% 重复
//! （clamp ±8）。回调可注入（测试用）；null = 默认写引擎总线。
//!
//! P15：sync 期间拖拽照常写 bus（引擎软接管判定是否生效——小步穿过
//! 当前速率带才暂时加减速）；微调在 sync 期间不写（防瞬移推子位置）。
//! 无论 sync 与否 thumb 恒显示引擎有效速率。

import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import '../engine/engine_controller.dart';

const double kTempoMin = -8.0;
const double kTempoMax = 8.0;

class TempoFader extends StatefulWidget {
  const TempoFader({
    super.key,
    required this.deck,
    this.onSetRate,
  });

  final DeckController deck;
  /// 变速写入（%）。null = 默认 engine.setRate。
  final void Function(double v)? onSetRate;

  @override
  State<TempoFader> createState() => _TempoFaderState();
}

class _TempoFaderState extends State<TempoFader> {
  Timer? _fineTimer;
  int _fineDir = 0;

  /// 推子区高度（拖拽计值用）：P18 起推子列高度由父级 IntrinsicHeight
  /// 撑高，不能用 LayoutBuilder（LayoutBuilder 不支持 intrinsic 计算），
  /// 改在拖拽时读自身尺寸。
  final _faderKey = GlobalKey();

  double _faderHeight() {
    final box = _faderKey.currentContext?.findRenderObject() as RenderBox?;
    return box?.size.height ?? 0;
  }

  void _setRate(double v) {
    final f = widget.onSetRate ??
        (v) => EngineController.instance.setRate(widget.deck.deck, v);
    f(clampDouble(v, kTempoMin, kTempoMax));
  }

  /// 微调按住：立即一次 + 每 100ms 重复（读侧绑快照 rate）。
  void _startFine(int dir) {
    _fineDir = dir;
    _fineTimer?.cancel();
    _applyFine();
    _fineTimer = Timer.periodic(
      const Duration(milliseconds: 100),
      (_) => _applyFine(),
    );
  }

  void _applyFine() {
    // P15：sync 期间微调不写——直接写会瞬移推子位置（暂时加减速用
    // nudge 键或推子软接管）。P14：基准用有效速率（引擎实际值）——
    // 取消 sync 后速率保持 sync 期间值、滑杆 bus 值与实际速率不一致，
    // 读 bus 值会瞬跳。
    if (widget.deck.syncOn.value) return;
    _setRate(widget.deck.effRate.value + 0.5 * _fineDir);
  }

  void _stopFine() {
    _fineTimer?.cancel();
    _fineTimer = null;
  }

  /// 推子位置 → 速率（上快下慢：dy=0 → +8，dy=h → −8）。
  /// P15：sync 期间拖拽照常写 bus——是否生效由引擎软接管判定（小步
  /// 穿过当前速率带才接管，触摸跳变不生效、需回位）。
  void _dragAt(double dy, double h) {
    if (h <= 0) return;
    final t = (dy / h).clamp(0.0, 1.0);
    _setRate(kTempoMax - t * (kTempoMax - kTempoMin));
  }

  @override
  void dispose() {
    _fineTimer?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final dc = widget.deck;
    return Column(
      children: [
        Expanded(
          child: ValueListenableBuilder<double>(
            // P14：thumb 恒显示有效速率（引擎实际值）——同步中锁定跟随
            // leader（拖 leader 推子实时跟随）；取消 sync 后速率保持
            // sync 期间值（推子仅解锁），thumb 不跳回滑杆位置、与音频
            // 一致（旧 P10.1 关 sync 回滑杆的"松手回弹"已删）。
            valueListenable: dc.effRate,
            builder: (_, rate, _) {
              return GestureDetector(
                key: _faderKey,
                behavior: HitTestBehavior.opaque,
                onVerticalDragStart: (d) =>
                    _dragAt(d.localPosition.dy, _faderHeight()),
                onVerticalDragUpdate: (d) =>
                    _dragAt(d.localPosition.dy, _faderHeight()),
                // 双击回正 0%
                onDoubleTap: () => _setRate(0.0),
                // 撑满跨轴：Expanded 给的是宽松宽约束，CustomPaint 无固有
                // 尺寸会收缩到 0 宽（拖拽/双击全部脱靶）
                child: SizedBox.expand(
                  child: CustomPaint(painter: _RatePainter(rate: rate)),
                ),
              );
            },
          ),
        ),
        const SizedBox(height: 8),
        Row(
          children: [
            Expanded(
              child: _HoldButton(
                label: '−',
                onDown: () => _startFine(-1),
                onUp: _stopFine,
              ),
            ),
            const SizedBox(width: 4),
            Expanded(
              child: _HoldButton(
                label: '+',
                onDown: () => _startFine(1),
                onUp: _stopFine,
              ),
            ),
          ],
        ),
      ],
    );
  }
}

/// 按住键：按下回调、松开/取消回 0（onTapDown/Up 无延迟，不做 timer）。
class _HoldButton extends StatelessWidget {
  const _HoldButton({
    required this.label,
    required this.onDown,
    required this.onUp,
  });

  final String label;
  final VoidCallback onDown;
  final VoidCallback onUp;

  @override
  Widget build(BuildContext context) {
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTapDown: (_) => onDown(),
      onTapUp: (_) => onUp(),
      onTapCancel: onUp,
      child: Container(
        height: 22,
        alignment: Alignment.center,
        decoration: BoxDecoration(
          color: const Color(0xFF2E353D),
          borderRadius: BorderRadius.circular(4),
        ),
        child: Text(
          label,
          style: const TextStyle(
            color: Colors.white70,
            fontSize: 11,
            fontWeight: FontWeight.bold,
          ),
        ),
      ),
    );
  }
}

/// 垂直速率推子自绘：track + 50% 处 0 刻度线 + 按 (rate+8)/16 定位的 thumb。
class _RatePainter extends CustomPainter {
  _RatePainter({required this.rate});

  final double rate;

  @override
  void paint(Canvas canvas, Size size) {
    final w = size.width;
    final h = size.height;
    // track
    final trackW = 10.0;
    final track = RRect.fromRectAndRadius(
      Rect.fromLTWH(w / 2 - trackW / 2, 0, trackW, h),
      const Radius.circular(5),
    );
    canvas.drawRRect(track, Paint()..color = const Color(0xFF2E353D));
    // 0 刻度线（50% 处）
    canvas.drawRect(
      Rect.fromLTWH(w / 2 - 6, h / 2 - 0.5, 12, 1),
      Paint()..color = Colors.white.withValues(alpha: 0.5),
    );
    // thumb：上快下慢
    final t = ((rate - kTempoMin) / (kTempoMax - kTempoMin)).clamp(0.0, 1.0);
    final thumbH = 12.0;
    final ty = h - t * h - thumbH / 2;
    canvas.drawRRect(
      RRect.fromRectAndRadius(
        Rect.fromLTWH(w / 2 - 11, ty, 22, thumbH),
        const Radius.circular(3),
      ),
      Paint()..color = const Color(0xFF3949AB),
    );
  }

  @override
  bool shouldRepaint(_RatePainter old) => old.rate != rate;
}
