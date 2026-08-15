//! P20 DeckFx：deck 主 FX 槽（slot 0）控制，整体放大填满行高
//! （kDeckFxHeight = kPadsRowHeight，旋钮 44、按钮 28、字号 12）。
//! P22.1 布局：删'强度'label（MixerKnob 空 label 不渲染）；右列两行按钮
//! 高度拉长 1.5 倍（28→42，kFxRowHeight）、行间紧贴（SizedBox 6、不撑满），
//! 左列高度 = 右列高度（2×42+6 = 90，kFxModuleHeight）——旋钮顶/开关底
//! 与右列行1顶/行2底 上下边界严格对齐，整块垂直居中于 116 行高。
//! 布局（旧版参照）：左列（强度旋钮 drywet，线性全扫角——0 值在 −150°、
//! 100% 在 +150°，见 MixerKnob.minAngleDeg + 下方 on/off 开关）+ 右侧
//! 两行三按钮：
//! - 行1（beat，参考 ManualLoop 行1 样式）：÷2 / 拍数显示（点击回 manifest
//!   默认拍数）/ ×2。仅 unit=="beats" 的参数（目前只有 gate.period）启用，
//!   其余效果禁用显示 '–'；
//! - 行2（fx 种类）：◀ 上一个 / 中间按钮（显示当前效果名：左键单击弹出
//!   选择菜单，P20 起右键事件改左键——enable 由开关负责）/ ▶ 下一个。
//! 选型（◀/▶/菜单）写 manifest 默认值到 p1..p4，同旧 FX RACK 面板 _selectFx；
//! 菜单选"无效果"（type 0）同时关 enable。
//!
//! 状态经 waveTick 重读总线（60Hz 驱动）；写入经 `PadActions` 出口
//! （默认走 EngineController/桥），测试注入假实现。

import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import '../engine/engine_controller.dart';
import '../src/rust/api.dart';
import 'deck_pads.dart';
import 'mixer_knob.dart';
import 'panel_button.dart';

/// P22.1 按钮行高：28 × 1.5（拉长 1.5 倍）。
const double kFxRowHeight = 42;
/// P22.1 左右列模块高 = 两行按钮紧贴（42+6+42）；左列旋钮+开关
/// 的上下边界与右列对齐，不撑满行高。
const double kFxModuleHeight = kFxRowHeight * 2 + 6;

/// 当前效果类型里 unit=='beats' 的参数（gate.period，p1）与位号；
/// 无拍参数效果（其余 7 种）返回 null。纯函数（测试直测）。
(int, FxParamWire)? fxBeatsParam(List<FxEffectWire> manifests, int type) {
  for (final m in manifests) {
    if (m.id != type) continue;
    for (var i = 0; i < m.params.length; i++) {
      if (m.params[i].unit == 'beats') return (i, m.params[i]);
    }
  }
  return null;
}

/// 效果类型循环：0 → 1..count → 0。纯函数（测试直测）。
int nextFxType(int type, int effectCount) {
  final n = effectCount;
  return type >= n ? 0 : type + 1;
}

/// 效果类型循环（上一个方向）：1 → 0 → count → count−1。纯函数（测试直测）。
int prevFxType(int type, int effectCount) {
  final n = effectCount;
  return type <= 0 ? n : type - 1;
}

/// 离散步进吸附（min/step/max 来自 manifest）。
double fxBeatSnap(double v, FxParamWire p) {
  final snapped = ((v - p.kindMin) / p.kindStep).round() * p.kindStep + p.kindMin;
  return snapped.clamp(p.kindMin, p.kindMax);
}

class DeckFx extends StatelessWidget {
  const DeckFx({
    super.key,
    required this.deck,
    this.actions = const PadActions(),
    this.slot = 0,
  });

  final DeckController deck;
  final PadActions actions;
  /// 控制的 FX 槽（0..7；默认 0 = deck 主效果）。
  final int slot;

  /// 选型：写 type + manifest 默认值到 p1..p4（同旧 FX RACK 面板
  /// _selectFx；type 0 关 enable）。◀/▶/菜单共用。
  void _selectType(DeckController dc, int id) {
    actions.setFxType(dc.deck, slot, id);
    if (id == 0) {
      actions.setFxEnable(dc.deck, slot, false);
      return;
    }
    for (final m in EngineController.instance.fxManifestsCache) {
      if (m.id != id) continue;
      for (var p = 0; p < m.params.length; p++) {
        actions.setFxParam(dc.deck, slot, p, m.params[p].defaultValue);
      }
    }
  }

  /// 选型菜单（P20 起名称按钮左键单击触发）：bottom sheet 列出 无效果 +
  /// 全部 manifest 效果，当前选中的打勾。
  void _showTypeMenu(BuildContext context, DeckController dc, int current) {
    final manifests = EngineController.instance.fxManifestsCache;
    showModalBottomSheet<int>(
      context: context,
      backgroundColor: const Color(0xFF1E232B),
      builder: (_) => SafeArea(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            for (final id in [0, ...manifests.map((m) => m.id)])
              ListTile(
                dense: true,
                title: Text(
                  id == 0 ? '无效果' : _effectLabel(manifests, id),
                  style: TextStyle(
                    color: id == 0 ? Colors.white38 : Colors.white,
                    fontSize: 13,
                  ),
                ),
                trailing: id == current
                    ? const Icon(Icons.check, size: 16, color: Color(0xFF6A1B9A))
                    : null,
                onTap: () => Navigator.pop(context, id),
              ),
          ],
        ),
      ),
    ).then((id) {
      if (id == null) return;
      _selectType(dc, id);
    });
  }

  @override
  Widget build(BuildContext context) {
    final dc = deck;
    final bus = 'Deck${dc.deck + 1}.fx${slot + 1}';
    final manifests = EngineController.instance.fxManifestsCache;
    return ValueListenableBuilder<int>(
      valueListenable: dc.waveTick,
      builder: (_, _, _) {
        final type = _safeBus('${bus}_type').round();
        final enabled = _safeBus('${bus}_enable') != 0;
        final beatsParam = fxBeatsParam(manifests, type);
        final beatIdx = beatsParam?.$1 ?? 0;
        final beatVal = beatsParam != null
            ? _safeBus('${bus}_p${beatIdx + 1}')
            : 0.0;
        // P22.3 重做左列宽度。根因：旧 Flexible(flex:1, loose) 参与 flex
        // 分配——左列约束 = 半列宽，FittedBox 只取 60px 但右列 Expanded
        // 只拿到一半 → 按钮与左列间留大片空白（220px 宽时 46px，"按钮没
        // 铺满"）；flex: 0 窄窗约束失效（左列不缩，右列取负溢出 5px）。
        // 方案：LayoutBuilder 直出（不能包 Row——flex 行给非 flex 子
        // unbounded 主轴向约束，内层 Expanded 会炸），左列上限 =
        // min(60, 半列)：宽窗 60 固定（右列 Expanded 拿满剩余全部宽度），
        // 窄窗两列等比收缩（P20.1 响应式保留，右列恒 ≥ 左列不溢出）。
        // 左列内容 P22.1：强度旋钮（0 值在 −150°，线性全扫角 300°，
        // 贴顶）+ on/off 开关（P20 enable 由开关负责，贴底），高度 =
        // kFxModuleHeight（与右列严格对齐），删'强度'label。
        return LayoutBuilder(
          builder: (context, c) {
            final leftMax = ((c.maxWidth - 8) / 2).clamp(0.0, 60.0);
            return Row(
              crossAxisAlignment: CrossAxisAlignment.center,
              children: [
                    ConstrainedBox(
                      constraints: BoxConstraints(maxWidth: leftMax),
                      child: FittedBox(
                        fit: BoxFit.scaleDown,
                        child: SizedBox(
                          width: 60,
                          height: kFxModuleHeight,
                          child: Column(
                            mainAxisAlignment: MainAxisAlignment.spaceBetween,
                            children: [
                              MixerKnob(
                                label: '',
                                min: 0,
                                max: 1,
                                minAngleDeg: -150,
                                initFromBus: '${bus}_drywet',
                                onChanged: (v) =>
                                    actions.setFxDrywet(dc.deck, slot, v),
                                size: 44,
                                color: const Color(0xFF6A1B9A),
                              ),
                              Switch(
                                value: enabled,
                                // P22.1 shrinkWrap：M3 padded 高 48，
                                // 44+48=92 > 模块 90 会溢出 2px——
                                // shrinkWrap 高 32，44+32=76 放进 90
                                // （spaceBetween 撑开），边界严格对齐。
                                materialTapTargetSize:
                                    MaterialTapTargetSize.shrinkWrap,
                                activeThumbColor: const Color(0xFF6A1B9A),
                                onChanged: (v) =>
                                    actions.setFxEnable(dc.deck, slot, v),
                              ),
                            ],
                          ),
                        ),
                      ),
                    ),
                const SizedBox(width: 8),
                Expanded(
                  child: _fxButtons(context, bus, manifests, type, enabled,
                      beatsParam, beatIdx, beatVal, dc),
                ),
              ],
            );
          },
        );
      },
    );
  }

  // ---- P22.3：右列两行按钮（LayoutBuilder 内 Expanded 拿满剩余宽度）----
  /// beat 行（÷2/拍数/×2）+ select 行（◀/名称/▶）——行内三按钮 Expanded
  /// 平分，横向铺满 fx 列内容（用户要求）。
  Widget _fxButtons(
    BuildContext context,
    String bus,
    List<FxEffectWire> manifests,
    int type,
    bool enabled,
    (int, FxParamWire)? beatsParam,
    int beatIdx,
    double beatVal,
    DeckController dc,
  ) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        // 行1：beat ÷2 / 显示（点击回默认拍数）/ ×2（loop 同款样式）
        Row(
          children: [
            Expanded(
              child: PanelButton(
                label: '÷2',
                height: kFxRowHeight,
                fontSize: 12,
                dead: beatsParam == null,
                onTap: beatsParam == null
                    ? null
                    : () {
                        actions.setFxParam(
                          dc.deck,
                          slot,
                          beatIdx,
                          fxBeatSnap(beatVal / 2, beatsParam.$2),
                        );
                      },
              ),
            ),
            const SizedBox(width: 4),
            Expanded(
              flex: 2,
              child: PanelButton(
                label: beatsParam == null ? '–' : _fmtBeat(beatVal),
                height: kFxRowHeight,
                fontSize: 12,
                dead: beatsParam == null,
                onTap: beatsParam == null
                    ? null
                    : () => actions.setFxParam(
                        dc.deck,
                        slot,
                        beatIdx,
                        beatsParam.$2.defaultValue,
                      ),
              ),
            ),
            const SizedBox(width: 4),
            Expanded(
              child: PanelButton(
                label: '×2',
                height: kFxRowHeight,
                fontSize: 12,
                dead: beatsParam == null,
                onTap: beatsParam == null
                    ? null
                    : () {
                        actions.setFxParam(
                          dc.deck,
                          slot,
                          beatIdx,
                          fxBeatSnap(beatVal * 2, beatsParam.$2),
                        );
                      },
              ),
            ),
          ],
        ),
        const SizedBox(height: 6),
        // 行2：◀ 上一个 / 中间（显示效果名，左键单击弹选型菜单——
        // P20 右键事件改左键；enable 看左列开关）/ ▶ 下一个。
        // 行1/行2 紧贴（SizedBox 6，旧方式），不撑满。
        Row(
          children: [
            Expanded(
              child: PanelButton(
                label: '◀',
                height: kFxRowHeight,
                fontSize: 12,
                onTap: () =>
                    _selectType(dc, prevFxType(type, manifests.length)),
              ),
            ),
            const SizedBox(width: 4),
            Expanded(
              flex: 2,
              child: PanelButton(
                label: _effectLabel(manifests, type),
                height: kFxRowHeight,
                fontSize: 12,
                onTap: () => _showTypeMenu(context, dc, type),
              ),
            ),
            const SizedBox(width: 4),
            Expanded(
              child: PanelButton(
                label: '▶',
                height: kFxRowHeight,
                fontSize: 12,
                onTap: () =>
                    _selectType(dc, nextFxType(type, manifests.length)),
              ),
            ),
          ],
        ),
      ],
    );
  }

  double _safeBus(String p) {
    try {
      return busGet(path: p);
    } catch (_) {
      return 0;
    }
  }

  String _effectLabel(List<FxEffectWire> manifests, int type) {
    if (type == 0) return 'FX';
    for (final m in manifests) {
      if (m.id == type) return m.label;
    }
    return 'FX';
  }

  /// 步进拍值不带多余小数：1.0 → '1'，0.75 → '0.75'。
  String _fmtBeat(double v) =>
      v == v.roundToDouble() ? v.toInt().toString() : v.toString();
}
