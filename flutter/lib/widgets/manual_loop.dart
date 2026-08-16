//! P18/P21/P23 ManualLoop：手动 loop 两行按钮（pad 上方、预览下方）。
//! 行1：÷2 / 显示当前 loop 拍数（点击激活/取消）/ ×2；
//! 行2：In（loop 开始点）/ Out（loop 结束点）。
//!
//! P23 手动定界：In/Out 都只写**原始播放头秒数**（不量化/不回拉），
//! 量化（起点/终点 snap 到 beatgrid 拍线、不足 1 拍保底整拍、无起点回拉
//! 4 拍）全部由引擎侧 snap_loop_bounds 完成（deck.rs update_params 块首
//! 检测 loop_in/out 总线边沿）——与 beatjump/loop pad 的网格同源，杜绝
//! P21 的 Flutter 静态 BPM 折算双源偏差（该侧量化已删）。
//! 默认拍数 4（Out 无有效 in 时引擎回拉用）；÷2/×2 修改本地拍数，激活中
//! 修改立即经 beatloop 重设（同 pad 语义）。
//! 动作经 `PadActions` 出口（默认走 EngineController/桥），测试注入假实现。
//!
//! P22-D：_toggle/_setIn/_setOut 用 onTapDown（同 beatjump P12 先例）——
//! In/Out/激活对时序敏感（激活瞬间的播放头位置即定界点），onTap 等手势
//! 仲裁延迟数十毫秒，位置就偏了。÷2/×2 改本地拍数不敏感，保持 onTap。

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

  /// P23 Out：loop_out = 当前位置**原始秒数**（不做任何折算）；量化与
  /// 起点回拉由引擎 snap_loop_bounds 完成。未激活 → 激活（引擎 bus
  /// 边沿检测进捕获）。
  void _setOut() {
    final dc = widget.deck;
    widget.actions.setLoopOut(dc.deck, dc.playhead.value);
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
                    onTapDown: _toggle,
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
            Expanded(child: PanelButton(label: 'In', onTapDown: _setIn)),
            const SizedBox(width: 4),
            Expanded(child: PanelButton(label: 'Out', onTapDown: _setOut)),
          ],
        ),
      ],
    );
  }
}
