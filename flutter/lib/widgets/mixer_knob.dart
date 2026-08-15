//! 调音台旋钮：按住拖拽改值（主导轴计值）、双击回 0。
//!
//! 值源二选一（互斥）：
//! - `value`：外部 ValueListenable（EQ 快照——60Hz tick 是唯一事实源，
//!   不建本地第二源）；
//! - `initFromBus`：mount 时 busGet 读总线一次 → 本地状态（gain/filter，
//!   引擎懒建总线默认 0；桥缺失/测试环境 try/catch 回 0）。
//! 不注册 onTap（双击判定会引入 300ms 单击延迟）；onDoubleTap 直接回 0。
//!
//! 文字单行：平时显示短标签，旋钮变动时（拖动中 / 双击后 600ms 闪值）显示数值。
//! 角度：0 值恒指 12 点（垂直向上），负/正半程各 150° 扫角。
//! 拖动：角度域计值——每侧 150° = [kKnobPixelsPerSide] px，所有旋钮
//! 「拖动距离 ↔ 旋钮角度」比例一致（EQ 非对称范围同样 70px/侧）。

import 'dart:async';
import 'dart:math' as math;

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../src/rust/api.dart';

/// 拖动距离 ↔ 旋钮角度全局比例：每侧（12 点 → 5/7 点）150° = 70px，
/// 全行程（7 点 → 5 点）140px。所有旋钮共用，不分范围对称性。
const double kKnobPixelsPerSide = 70;

/// 0 值恒指 12 点（−90°）：负/正半程各 150°（5π/6），分段线性。
double knobAngle(double v, double min, double max) {
  if (v >= 0) {
    final t = max > 0 ? (v / max).clamp(0.0, 1.0) : 0.0;
    return -math.pi / 2 + t * 5 * math.pi / 6; // 12 点 → 5 点（+60°）
  }
  final t = min < 0 ? (v / min).clamp(0.0, 1.0) : 0.0;
  return -math.pi / 2 - t * 5 * math.pi / 6; // 12 点 → 7 点（−240° ≡ +120°）
}

/// [knobAngle] 的逆映射（角度 → 值，clamp 到 [min,max]）。
double knobValue(double a, double min, double max) {
  if (a >= -math.pi / 2) {
    final t = ((a + math.pi / 2) / (5 * math.pi / 6)).clamp(0.0, 1.0);
    return t * max;
  }
  final t = ((-math.pi / 2 - a) / (5 * math.pi / 6)).clamp(0.0, 1.0);
  return t * min;
}

/// 线性全扫角：min → [minAngleDeg] 度，max → minAngleDeg+300°（5π/3）。
/// 全正范围（干湿/强度）旋钮用——0 值在 −150°（左端）、100% 在 +150°
/// （右端），0 值锚点结构（12 点）对无负值范围无意义。
double knobAngleLinear(double v, double min, double max, double minAngleDeg) {
  final t = max > min ? ((v - min) / (max - min)).clamp(0.0, 1.0) : 0.0;
  return minAngleDeg * math.pi / 180 + t * 5 * math.pi / 3;
}

/// [knobAngleLinear] 的逆映射（角度 → 值，clamp 到 [min,max]）。
double knobValueLinear(double a, double min, double max, double minAngleDeg) {
  final t = ((a - minAngleDeg * math.pi / 180) / (5 * math.pi / 3))
      .clamp(0.0, 1.0);
  return min + t * (max - min);
}

class MixerKnob extends StatefulWidget {
  const MixerKnob({
    super.key,
    required this.label,
    required this.min,
    required this.max,
    required this.onChanged,
    this.value,
    this.initFromBus,
    this.size = 44,
    this.color = const Color(0xFF1E88E5),
    this.format,
    this.minAngleDeg,
  }) : assert(value == null || initFromBus == null, 'value 与 initFromBus 二选一');

  final String label;
  final double min;
  final double max;
  final ValueChanged<double> onChanged;
  /// 旋钮直径。
  final double size;
  /// 外部值源（EQ 快照）。与 initFromBus 互斥。
  final ValueListenable<double>? value;
  /// 总线路径：mount 时读一次初始化本地状态。与 value 互斥。
  final String? initFromBus;
  final Color color;
  /// 值文本格式化（null = 一位小数）。
  final String Function(double v)? format;
  /// 线性全扫角起点（度，P18.1）：非 null 时 [min,max] 线性映射到
  /// minAngleDeg..minAngleDeg+300°（0 值在 minAngleDeg，如 FX 干湿 −150°）；
  /// null = 现状 0 值恒指 12 点（EQ 等含负值范围保持）。
  final double? minAngleDeg;

  @override
  State<MixerKnob> createState() => _MixerKnobState();
}

class _MixerKnobState extends State<MixerKnob> {
  double? _local;
  /// 显示数值（true）还是标签（false）。拖动中为 true，pan 结束回标签；
  /// 双击置 true 后由 [_hideTimer] 600ms 恢复。
  bool _showValue = false;
  Timer? _hideTimer;

  @override
  void initState() {
    super.initState();
    // 无外部值源（纯本地 / initFromBus）→ 本地状态起始 0（busGet 覆盖之）
    if (widget.value == null) {
      double v = 0;
      final path = widget.initFromBus;
      if (path != null) {
        try {
          v = busGet(path: path).clamp(widget.min, widget.max);
        } catch (_) {
          // 桥缺失（测试）→ 0
        }
      }
      _local = v;
    }
  }

  @override
  void dispose() {
    _hideTimer?.cancel();
    super.dispose();
  }

  double _cur() => widget.value?.value ?? _local!;

  void _set(double v) {
    final cv = clampDouble(v, widget.min, widget.max);
    widget.onChanged(cv);
    // 无条件 setState：外部值源模式也要重建文字（值仍读 notifier，无第二值源）
    setState(() {
      _showValue = true;
      if (widget.value == null) _local = cv;
    });
  }

  /// 双击回 0 + 闪值 600ms 后回标签。
  void _flashReset() {
    _set(0.0);
    _hideTimer?.cancel();
    _hideTimer = Timer(const Duration(milliseconds: 600), () {
      if (mounted) setState(() => _showValue = false);
    });
  }

  /// 当前旋钮的角度映射（线性扫角 or 0 值 12 点锚点结构）。
  double _angleFor(double v) => widget.minAngleDeg == null
      ? knobAngle(v, widget.min, widget.max)
      : knobAngleLinear(v, widget.min, widget.max, widget.minAngleDeg!);

  double _valueFor(double a) => widget.minAngleDeg == null
      ? knobValue(a, widget.min, widget.max)
      : knobValueLinear(a, widget.min, widget.max, widget.minAngleDeg!);

  /// 主导轴计值：竖直拖 = 上推加值，水平拖 = 右拉加值。
  /// 角度域增量（每侧 150° = kKnobPixelsPerSide px）→ 逆映射回值，
  /// 保证所有旋钮「拖动距离 ↔ 旋钮角度」比例一致。
  void _drag(Offset delta) {
    final d = delta.dy.abs() >= delta.dx.abs() ? -delta.dy : delta.dx;
    final a = _angleFor(_cur()) +
        d * (5 * math.pi / 6) / kKnobPixelsPerSide;
    _set(_valueFor(a));
  }

  String _fmt(double v) =>
      (widget.format ?? (v) => v.toStringAsFixed(1))(v);

  @override
  Widget build(BuildContext context) {
    final listenable = widget.value;
    if (listenable == null) return _build(_local!);
    return ValueListenableBuilder<double>(
      valueListenable: listenable,
      builder: (_, v, _) => _build(v),
    );
  }

  Widget _build(double v) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        GestureDetector(
          behavior: HitTestBehavior.opaque,
          onPanStart: (_) => setState(() => _showValue = true),
          onPanUpdate: (d) => _drag(d.delta),
          onPanEnd: (_) => setState(() => _showValue = false),
          onPanCancel: () => setState(() => _showValue = false),
          onDoubleTap: _flashReset,
          child: SizedBox(
            width: widget.size,
            height: widget.size,
            child: CustomPaint(
              painter: _KnobPainter(
                value: v,
                min: widget.min,
                max: widget.max,
                color: widget.color,
                minAngleDeg: widget.minAngleDeg,
              ),
            ),
          ),
        ),
        // 单行文字：变动时显值、平时显标签
        Text(
          _showValue ? _fmt(v) : widget.label,
          style: _showValue
              ? TextStyle(
                  color: widget.color.withValues(alpha: 0.9),
                  fontSize: 10,
                  fontFeatures: const [FontFeature.tabularFigures()],
                )
              : const TextStyle(color: Colors.white54, fontSize: 9),
        ),
      ],
    );
  }
}

/// 旋钮自绘：圆身 + 值弧（0 → 当前值）+ 指示线 + 12 点方向中心刻度 +
/// 中心附近亮点。
///
/// 角度约定：0 值恒指 12 点（−90°）；负值沿逆时针 150° 至 7 点方向
/// （−240°≡+120°），正值沿顺时针 150° 至 5 点方向（+60°）——
/// 非对称范围（EQ −40..+6）下 0 仍在 12 点，正侧单位角度更细。
class _KnobPainter extends CustomPainter {
  _KnobPainter({
    required this.value,
    required this.min,
    required this.max,
    required this.color,
    this.minAngleDeg,
  });

  final double value;
  final double min;
  final double max;
  final Color color;
  /// 线性扫角起点（null = 0 值恒指 12 点，见 [_angle]）。
  final double? minAngleDeg;

  /// 0 值恒指 12 点（−90°）：负/正半程各 150°（5π/6），分段线性（见
  /// [knobAngle]）；minAngleDeg 非 null 时改为线性全扫（见 [knobAngleLinear]）。
  double _angle(double v) => minAngleDeg == null
      ? knobAngle(v, min, max)
      : knobAngleLinear(v, min, max, minAngleDeg!);

  @override
  void paint(Canvas canvas, Size size) {
    final c = size.center(Offset.zero);
    final r = size.shortestSide / 2 - 2;
    // 圆身 + 描边
    canvas.drawCircle(c, r, Paint()..color = const Color(0xFF2E353D));
    canvas.drawCircle(
      c,
      r,
      Paint()
        ..color = Colors.white.withValues(alpha: 0.08)
        ..style = PaintingStyle.stroke
        ..strokeWidth = 1,
    );
    final a0 = _angle(0.0);
    final av = _angle(value);
    // 值弧（0 点 → 当前值）
    if ((value - 0.0).abs() > (max - min) * 0.005) {
      canvas.drawArc(
        Rect.fromCircle(center: c, radius: r - 3),
        a0,
        av - a0,
        false,
        Paint()
          ..color = color
          ..style = PaintingStyle.stroke
          ..strokeWidth = 2.5,
      );
    }
    // 指示线
    canvas.drawLine(
      c,
      c + Offset.fromDirection(av, r * 0.7),
      Paint()
        ..color = Colors.white.withValues(alpha: 0.75)
        ..strokeWidth = 2,
    );
    // 12 点方向中心刻度
    canvas.drawLine(
      c + Offset.fromDirection(a0, r - 4),
      c + Offset.fromDirection(a0, r - 0.5),
      Paint()
        ..color = Colors.white.withValues(alpha: 0.25)
        ..strokeWidth = 2,
    );
    // 中心亮点
    if (value.abs() < (max - min) * 0.005) {
      canvas.drawCircle(c, 1.8, Paint()..color = color);
    }
  }

  @override
  bool shouldRepaint(_KnobPainter old) =>
      old.value != value ||
      old.min != min ||
      old.max != max ||
      old.color != color ||
      old.minAngleDeg != minAngleDeg;
}
