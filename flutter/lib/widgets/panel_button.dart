//! P18 面板通用按钮：与 pad 区统一风格（0xFF2E353D 底、圆角 4、白字）。
//! ManualLoop / BeatJumpPanel / DeckFx 共用。点击或按住（onTapDown 立即
//! 触发——beatjump 时序敏感）二选一；lit = 激活色底。
//! P18.1：加 onSecondaryTap（右键）；P20：加 fontSize（DeckFx 放大）。

import 'package:flutter/material.dart';

class PanelButton extends StatelessWidget {
  const PanelButton({
    super.key,
    required this.label,
    this.onTap,
    this.onTapDown,
    this.onSecondaryTap,
    this.lit = false,
    this.litColor = const Color(0xFF2E7D32),
    this.height = 30,
    this.dead = false,
    this.fontSize = 11,
  });

  final String label;
  final VoidCallback? onTap;
  final VoidCallback? onTapDown;
  /// 右键回调（桌面 kSecondaryMouseButton）。
  final VoidCallback? onSecondaryTap;
  final bool lit;
  final Color litColor;
  final double height;
  /// 禁用（灰底淡字，不可点）。
  final bool dead;
  /// 字号（P20 DeckFx 放大用）。
  final double fontSize;

  @override
  Widget build(BuildContext context) {
    final Color bg;
    if (dead) {
      bg = const Color(0xFF1E232B);
    } else if (lit) {
      bg = litColor;
    } else {
      bg = const Color(0xFF2E353D);
    }
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: onTap,
      onTapDown: onTapDown == null ? null : (_) => onTapDown!(),
      onSecondaryTap: onSecondaryTap,
      child: Container(
        height: height,
        alignment: Alignment.center,
        decoration: BoxDecoration(
          color: bg,
          borderRadius: BorderRadius.circular(4),
        ),
        // P20 响应式：FittedBox 防窄窗文字溢出（DeckFx 放大后 240px 回归）
        child: FittedBox(
          fit: BoxFit.scaleDown,
          child: Text(
            label,
            style: TextStyle(
              color: dead
                  ? Colors.white12
                  : (lit ? Colors.white : Colors.white70),
              fontSize: fontSize,
              fontWeight: FontWeight.w600,
            ),
          ),
        ),
      ),
    );
  }
}
