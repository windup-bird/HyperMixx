//! P18/P21 ManualLoop：手动 loop 两行按钮（pad 上方、预览下方）。
//! 行1：÷2 / 显示当前 loop 拍数（点击激活/取消）/ ×2；
//! 行2：In（loop 开始点）/ Out（loop 结束点）。
//!
//! P21 手动定界：In 只写 loop_in（不激活、不回填）；Out 写 loop_out 并
//! **整倍量化**（out = in + 整拍数×拍长，长度恒为整拍、与节拍对齐）后
//! 激活（P18 引擎总线边沿检测进捕获，零桥改动，见 deck.rs update_params）。
//! 默认拍数 4（Out 无有效 in 回拉时用）；÷2/×2 修改本地拍数，激活中修改
//! 立即经 beatloop 重设（同 pad 语义）。
//! 动作经 `PadActions` 出口（默认走 EngineController/桥），测试注入假实现。

import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import 'deck_pads.dart';
import 'panel_button.dart';

/// 拍数范围（与 pad loop 列表 1/32..64 一致）。
const double kManualLoopMinBeats = 1 / 32;
const double kManualLoopMaxBeats = 64;

/// P20 拍数显示：只显示分数或整数，且只显示数字（无"拍"后缀）。
/// - 整数（去浮点噪声，如总线秒数 ×bpm/60 的 ~1e-6 误差）：4.0000001 → '4'；
/// - ≤32 分母分数：0.5 → '1/2'、0.75 → '3/4'、1/32 → '1/32'；
/// - 无法 ≤32 分母表达（理论上仅手动 In/Out 极端值）：两位小数兜底。
String fmtBeats(double b) {
  if (b <= 0 || !b.isFinite) return '0';
  final v = (b * 1e6).round() / 1e6;
  if ((v - v.round()).abs() < 1e-6) return v.round().toString();
  for (var d = 2; d <= 32; d++) {
    final n = v * d;
    if ((n - n.round()).abs() < 1e-6) return '${n.round()}/$d';
  }
  return v.toStringAsFixed(2);
}

class ManualLoop extends StatefulWidget {
  const ManualLoop({
    super.key,
    required this.deck,
    this.actions = const PadActions(),
  });

  final DeckController deck;
  final PadActions actions;

  @override
  State<ManualLoop> createState() => _ManualLoopState();
}

class _ManualLoopState extends State<ManualLoop> {
  double _beats = 4;

  /// 拍 → 秒：用静态 BPM（loop pad 同款折算）；无网格回退 120
  /// （与引擎 set_beat_loop 同源折算，P20 显示不再出现 BPM 双源偏差）。
  double get _beatSecs {
    final bpm = widget.deck.bpm.value;
    return 60.0 / (bpm > 0 ? bpm : 120.0);
  }

  void _setBeats(double b) {
    setState(() {
      _beats = b.clamp(kManualLoopMinBeats, kManualLoopMaxBeats);
    });
    // 激活中 ÷2/×2：立即按新拍数重设 beat loop（同 pad 语义）
    if (widget.deck.loopActive.value) {
      widget.actions.activateBeatLoop(widget.deck.deck, _beats);
    }
  }

  void _toggle() {
    final dc = widget.deck;
    if (dc.loopActive.value) {
      widget.actions.setLoopActive(dc.deck, false);
    } else {
      widget.actions.activateBeatLoop(dc.deck, _beats);
    }
  }

  /// P21 In：只写 loop_in = 当前位置（手动定下界）——不再自动回填 out、
  /// 不再自动激活；由 Out 确定上界并激活。
  void _setIn() {
    final dc = widget.deck;
    widget.actions.setLoopIn(dc.deck, dc.playhead.value);
  }

  /// P21 Out：loop_out = 当前位置**整倍量化**（相对 in 取整拍：
  /// out = in + max(1, round((pos−in)/拍长))×拍长——手动定上界后长度
  /// 恒为整拍，循环与节拍对齐）；无有效 in（未设 = 0，或 in ≥ pos）时
  /// 无量化基准 → 保持原值并回拉 in = pos − 默认拍长。未激活 → 激活
  /// （引擎 bus 边沿检测进捕获）。
  void _setOut() {
    final dc = widget.deck;
    final pos = dc.playhead.value;
    final inValid = dc.loopIn.value > 0 && dc.loopIn.value < pos - 1e-6;
    if (inValid) {
      final beats = ((pos - dc.loopIn.value) / _beatSecs).round().clamp(1, 1 << 30);
      final out = dc.loopIn.value + beats * _beatSecs;
      widget.actions.setLoopOut(dc.deck, out);
    } else {
      widget.actions.setLoopOut(dc.deck, pos);
      final inPos = pos - _beats * _beatSecs;
      widget.actions.setLoopIn(dc.deck, inPos > 0 ? inPos : 0.0);
    }
    if (!dc.loopActive.value) widget.actions.setLoopActive(dc.deck, true);
  }

  @override
  Widget build(BuildContext context) {
    final dc = widget.deck;
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        Row(
          children: [
            Expanded(
              child: PanelButton(
                label: '÷2',
                onTap: () => _setBeats(_beats / 2),
              ),
            ),
            const SizedBox(width: 4),
            Expanded(
              flex: 2,
              child: ValueListenableBuilder<bool>(
                valueListenable: dc.loopActive,
                builder: (_, active, _) {
                  // 激活中显示实际环拍数（实时快照按 _beatSecs 折算，与
                  // 引擎拍长同源）；未激活显示目标拍数。
                  // P20：fmtBeats 只显示分数/整数（去小数噪声、无"拍"）。
                  final cur = active
                      ? (dc.loopOut.value - dc.loopIn.value) / _beatSecs
                      : 0.0;
                  final beats = active && cur > 0 ? cur : _beats;
                  return PanelButton(
                    label: fmtBeats(beats),
                    lit: active,
                    litColor: const Color(0xFF2E7D32),
                    onTap: _toggle,
                  );
                },
              ),
            ),
            const SizedBox(width: 4),
            Expanded(
              child: PanelButton(
                label: '×2',
                onTap: () => _setBeats(_beats * 2),
              ),
            ),
          ],
        ),
        const SizedBox(height: 6),
        Row(
          children: [
            Expanded(child: PanelButton(label: 'In', onTap: _setIn)),
            const SizedBox(width: 4),
            Expanded(child: PanelButton(label: 'Out', onTap: _setOut)),
          ],
        ),
      ],
    );
  }
}
