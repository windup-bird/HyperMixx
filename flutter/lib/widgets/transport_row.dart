//! P19/P20 transport 行（playpanel）：6 等分按钮，统一样式（高 30、圆角 4、
//! Expanded 平分横向空间，文字 FittedBox 防窄窗溢出）。
//! P20 顺序：PLAY / CUE / SYNC / SHIFT / << / >>：
//! - PLAY：播放/暂停切换（P22.4 实心橙色 0xFFFF7043——原 CUE 色，
//!   文字改符号：播放 ▶ / 暂停 ‖）；
//! - CUE：主 cue 按钮（P22.4 改 amber 0xFFFFB300——让出橙色给 PLAY；
//!   P19 起播放时点击 = 暂停并回到 cue 点，hotcue 保持"召回继续播"）；
//! - SYNC：点击切换（P22.4 改 teal 0xFF00897B；P22.4 deckinfo 列加回
//!   同名按钮——此处保留 P16 leader（master deck）amber 边框判定）；
//! - SHIFT：占位死键（未来 shift 组合功能预留，灰死不可点）；
//! - <<：加速（nudge +1，按住语义）；>>：减速（nudge −1，P17.1 互换）。
//! 前四个状态色，<< >> 中性灰。全部动作经 PadActions 出口（测试注入假实现）。

import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import '../engine/engine_controller.dart';
import 'deck_pads.dart';

/// P16 leader（master deck）判定——与引擎 beat sync 规则一致：
/// 单开 = 不开 sync 的轨；双开 = deck0；都关无 leader。
bool isSyncLeader(int deck) {
  final decks = EngineController.instance.decks;
  final s0 = decks[0].syncOn.value;
  final s1 = decks[1].syncOn.value;
  if (s0 && s1) return deck == 0;
  if (s1) return deck == 0;
  if (s0) return deck == 1;
  return false;
}

/// P19 transport 行（原 play/cue/nudge 行重构）。
class TransportRow extends StatelessWidget {
  const TransportRow({
    super.key,
    required this.deck,
    this.actions = const PadActions(),
  });

  final DeckController deck;
  final PadActions actions;

  @override
  Widget build(BuildContext context) {
    final dc = deck;
    return SizedBox(
      height: 30,
      child: Row(
        children: [
          // P20 顺序：PLAY CUE SYNC SHIFT << >>（P22.4 PLAY 改橙底符号）
          Expanded(
            child: ValueListenableBuilder<bool>(
              valueListenable: dc.playing,
              builder: (_, playing, _) {
                return _TransportButton(
                  label: playing ? '‖' : '▶',
                  active: playing,
                  activeColor: const Color(0xFFFF7043),
                  onTap: () => actions.setPlaying(dc.deck, !playing),
                );
              },
            ),
          ),
          const SizedBox(width: 4),
          Expanded(child: CueButton(deck: dc, actions: actions)),
          const SizedBox(width: 4),
          // P16：leader 判定依赖两轨 sync 状态——监听双方，任一变化重绘
          //（否则另一轨开关 sync 后本面板的 leader 指示不刷新）
          Expanded(
            child: ListenableBuilder(
              listenable: Listenable.merge([
                dc.syncOn,
                EngineController.instance.decks[1 - dc.deck].syncOn,
              ]),
              builder: (context, _) {
                final on = dc.syncOn.value;
                return _TransportButton(
                  label: 'SYNC',
                  active: on,
                  activeColor: const Color(0xFF00897B),
                  isLeader: isSyncLeader(dc.deck),
                  onTap: () => actions.setSync(dc.deck, !on),
                );
              },
            ),
          ),
          const SizedBox(width: 4),
          Expanded(
            child: _TransportButton(label: 'SHIFT', dead: true),
          ),
          const SizedBox(width: 4),
          Expanded(
            child: _TransportButton(
              label: '<<',
              onDown: () => actions.setNudge(dc.deck, 1),
              onUp: () => actions.setNudge(dc.deck, 0),
            ),
          ),
          const SizedBox(width: 4),
          Expanded(
            child: _TransportButton(
              label: '>>',
              onDown: () => actions.setNudge(dc.deck, -1),
              onUp: () => actions.setNudge(dc.deck, 0),
            ),
          ),
        ],
      ),
    );
  }
}

/// 统一 transport 按钮：高 30、圆角 4；激活 = 状态色底白字，死键 = 暗底
/// 暗字，leader = amber 边框（P16）。onTap = 点击；onDown/onUp = 按住
/// 语义（nudge 引擎侧保持，松开写 0）。
class _TransportButton extends StatelessWidget {
  const _TransportButton({
    required this.label,
    this.onTap,
    this.onDown,
    this.onUp,
    this.active = false,
    this.activeColor,
    this.dead = false,
    this.isLeader = false,
  });

  final String label;
  final VoidCallback? onTap;
  final VoidCallback? onDown;
  final VoidCallback? onUp;
  final bool active;
  final Color? activeColor;
  final bool dead;
  final bool isLeader;

  @override
  Widget build(BuildContext context) {
    final Color bg;
    if (dead) {
      bg = const Color(0xFF1E232B);
    } else if (active) {
      bg = activeColor ?? const Color(0xFF2E353D);
    } else {
      bg = const Color(0xFF2E353D);
    }
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTapDown: onDown != null ? (_) => onDown!() : null,
      onTapUp: onUp != null ? (_) => onUp!() : null,
      onTapCancel: onUp != null ? () => onUp!() : null,
      onTap: onTap,
      child: Container(
        alignment: Alignment.center,
        decoration: BoxDecoration(
          color: bg,
          borderRadius: BorderRadius.circular(4),
          border: isLeader
              ? Border.all(color: const Color(0xFFFFB300), width: 1.5)
              : null,
        ),
        child: FittedBox(
          fit: BoxFit.scaleDown,
          child: Text(
            label,
            style: TextStyle(
              color: dead
                  ? Colors.white12
                  : active
                      ? Colors.white
                      : isLeader
                          ? const Color(0xFFFFB300)
                          : Colors.white70,
              fontSize: 11,
              fontWeight: FontWeight.bold,
            ),
          ),
        ),
      ),
    );
  }
}
