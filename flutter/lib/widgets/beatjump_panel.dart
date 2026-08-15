//! P18 BeatJumpPanel：与 ManualLoop 同款式两行按钮（并排、宽度平分）。
//! 行1：÷2 / 显示跳拍数（点击回默认 4 拍）/ ×2；
//! 行2：◀ 左跳 / ▶ 右跳。
//!
//! onTapDown 按下即跳（P12：beatjump 时序敏感，onTap 等手势仲裁会丢相位）；
//! 跳距 = 本地拍数 × 拍长（引擎精确距离语义，P16/P17.1 定格）。
//! 动作经 `PadActions` 出口（默认走 EngineController/桥），测试注入假实现。

import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import 'deck_pads.dart';
import 'manual_loop.dart';
import 'panel_button.dart';

/// 跳拍范围（整拍）。
const double kBeatJumpMinBeats = 1;
const double kBeatJumpMaxBeats = 32;

class BeatJumpPanel extends StatefulWidget {
  const BeatJumpPanel({
    super.key,
    required this.deck,
    this.actions = const PadActions(),
  });

  final DeckController deck;
  final PadActions actions;

  @override
  State<BeatJumpPanel> createState() => _BeatJumpPanelState();
}

class _BeatJumpPanelState extends State<BeatJumpPanel> {
  double _beats = 4;

  void _setBeats(double b) {
    setState(() {
      // 整拍域（÷2 到 1 为止，×2 到 32 为止）
      _beats = b.clamp(kBeatJumpMinBeats, kBeatJumpMaxBeats).roundToDouble();
    });
  }

  void _jump(double dir) {
    widget.actions.beatJump(widget.deck.deck, dir * _beats);
  }

  @override
  Widget build(BuildContext context) {
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
              child: PanelButton(
                // P20：只显示数字（fmtBeats，无"拍"后缀）
                label: '±${fmtBeats(_beats)}',
                onTap: () => _setBeats(4), // 点击回默认 4 拍
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
            Expanded(
              child: PanelButton(
                label: '◀',
                onTapDown: () => _jump(-1),
              ),
            ),
            const SizedBox(width: 4),
            Expanded(
              child: PanelButton(
                label: '▶',
                onTapDown: () => _jump(1),
              ),
            ),
          ],
        ),
      ],
    );
  }
}
