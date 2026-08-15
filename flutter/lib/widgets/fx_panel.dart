//! FX RACK 面板（bottom sheet）：8 槽 ×（效果下拉 + enable + dry/wet + manifest 参数）。
//! 效果/参数完全由桥的 `fx_manifests()` 驱动（manifest 是唯一来源）；
//! 选型时把 manifest 默认值写入总线 p1..p4（引擎读原始总线值，不自己套默认）。
//! 槽初值从总线读回（重开面板反映真实引擎状态）。
//! P8：enable 只受 ON 开关控制（引擎不再随换型强制置 1）。

import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import '../engine/engine_controller.dart';
import '../src/rust/api.dart';

class FxPanel extends StatelessWidget {
  const FxPanel({super.key, required this.deck, this.initialSlot = 0});

  final DeckController deck;

  /// FX pad 右键进入时高亮定位的槽（0..7，无滚动定位，仅标题提示）。
  final int initialSlot;

  /// 从 deck 面板弹出 FX 架。
  static void show(
    BuildContext context,
    DeckController deck, {
    int initialSlot = 0,
  }) {
    showModalBottomSheet<void>(
      context: context,
      backgroundColor: const Color(0xFF1E232B),
      isScrollControlled: true,
      builder: (_) => SafeArea(
        child: SingleChildScrollView(
          padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 14),
          child: FxPanel(deck: deck, initialSlot: initialSlot),
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final manifests = EngineController.instance.fxManifestsCache;
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const Text('FX RACK',
            style: TextStyle(color: Colors.white, fontSize: 13, fontWeight: FontWeight.w600)),
        const SizedBox(height: 4),
        if (manifests.isEmpty)
          const Padding(
            padding: EdgeInsets.only(top: 8),
            child: Text('引擎未连接（无效果清单）',
                style: TextStyle(color: Colors.white38, fontSize: 11)),
          )
        else
          for (var s = 0; s < 8; s++) ...[
            _FxSlot(
              key: ValueKey('slot$s'),
              deck: deck,
              slot: s,
              highlighted: s == initialSlot,
            ),
            if (s < 7) const SizedBox(height: 10),
          ],
      ],
    );
  }
}

class _FxSlot extends StatefulWidget {
  const _FxSlot({
    super.key,
    required this.deck,
    required this.slot,
    this.highlighted = false,
  });

  final DeckController deck;
  final int slot;
  final bool highlighted;

  @override
  State<_FxSlot> createState() => _FxSlotState();
}

class _FxSlotState extends State<_FxSlot> {
  final EngineController _engine = EngineController.instance;
  int _fxId = 0;
  bool _enable = false;
  double _drywet = 0;
  final List<double> _params = [0, 0, 0, 0];

  String get _slotBus =>
      'Deck${widget.deck.deck + 1}.fx${widget.slot + 1}';

  @override
  void initState() {
    super.initState();
    // 从总线读回真实引擎状态；桥缺失（测试环境）时静默用默认值。
    double safeBus(String p) {
      try {
        return busGet(path: p);
      } catch (_) {
        return 0;
      }
    }

    _fxId = safeBus('${_slotBus}_type').round();
    _enable = safeBus('${_slotBus}_enable') != 0;
    _drywet = safeBus('${_slotBus}_drywet');
    for (var p = 0; p < 4; p++) {
      _params[p] = safeBus('${_slotBus}_p${p + 1}');
    }
  }

  FxEffectWire? get _effect => _manifestById(_fxId);

  FxEffectWire? _manifestById(int id) {
    for (final m in _engine.fxManifestsCache) {
      if (m.id == id) return m;
    }
    return null;
  }

  void _selectFx(int id) {
    setState(() {
      _fxId = id;
      _engine.setFxType(widget.deck.deck, widget.slot, id);
      if (id == 0) {
        _enable = false;
        _engine.setFxEnable(widget.deck.deck, widget.slot, false);
      } else {
        // 选型：把 manifest 默认值写入 p1..p4（引擎不自己套默认）
        final ef = _manifestById(id);
        for (var p = 0; p < 4; p++) {
          final dflt =
              (ef != null && p < ef.params.length) ? ef.params[p].defaultValue : 0.0;
          _params[p] = dflt;
          _engine.setFxParam(widget.deck.deck, widget.slot, p, dflt);
        }
      }
    });
  }

  void _toggleEnable(bool on) {
    setState(() => _enable = on);
    _engine.setFxEnable(widget.deck.deck, widget.slot, on);
  }

  @override
  Widget build(BuildContext context) {
    final effect = _effect;
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 8),
      decoration: BoxDecoration(
        color: widget.highlighted
            ? const Color(0xFF2E3A52)
            : const Color(0xFF23282F),
        borderRadius: BorderRadius.circular(6),
        border: widget.highlighted
            ? Border.all(color: const Color(0xFF3949AB))
            : null,
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Text('FX${widget.slot + 1}',
                  style: const TextStyle(color: Colors.white38, fontSize: 11)),
              const SizedBox(width: 8),
              // 效果下拉
              Expanded(
                child: DropdownButton<int>(
                  value: _fxId == 0 ? null : _fxId,
                  isExpanded: true,
                  dropdownColor: const Color(0xFF2E353D),
                  hint: const Text('无效果',
                      style: TextStyle(color: Colors.white38, fontSize: 12)),
                  style: const TextStyle(color: Colors.white, fontSize: 12),
                  underline: const SizedBox.shrink(),
                  items: [
                    const DropdownMenuItem(value: 0, child: Text('无效果')),
                    for (final m in _engine.fxManifestsCache)
                      DropdownMenuItem(value: m.id, child: Text(m.label)),
                  ],
                  onChanged: (v) => _selectFx(v ?? 0),
                ),
              ),
              const SizedBox(width: 8),
              Text('ON', style: const TextStyle(color: Colors.white38, fontSize: 10)),
              Switch(
                value: _enable,
                activeThumbColor: const Color(0xFF3949AB),
                onChanged: _toggleEnable,
                materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
              ),
            ],
          ),
          if (effect != null) ...[
            const SizedBox(height: 2),
            // dry/wet
            Row(
              children: [
                const Text('Dry/Wet', style: TextStyle(color: Colors.white38, fontSize: 10)),
                Expanded(
                  child: SliderTheme(
                    data: SliderTheme.of(context).copyWith(
                      activeTrackColor: const Color(0xFF9CCC65),
                      thumbColor: const Color(0xFF9CCC65),
                      inactiveTrackColor: const Color(0xFF2E353D),
                    ),
                    child: Slider(
                      value: _drywet.clamp(0.0, 1.0),
                      onChanged: (v) {
                        setState(() => _drywet = v);
                        _engine.setFxDrywet(widget.deck.deck, widget.slot, v);
                      },
                    ),
                  ),
                ),
                SizedBox(
                  width: 34,
                  child: Text('${(_drywet * 100).round()}%',
                      textAlign: TextAlign.right,
                      style: const TextStyle(color: Colors.white54, fontSize: 10)),
                ),
              ],
            ),
            // manifest 参数
            for (var p = 0; p < effect.params.length; p++)
              _paramRow(context, effect.params[p], p),
          ],
        ],
      ),
    );
  }

  Widget _paramRow(BuildContext context, FxParamWire spec, int idx) {
    final value = _params[idx].clamp(spec.kindMin, spec.kindMax);
    final stepped = spec.kindStepped;
    final divisions =
        stepped ? ((spec.kindMax - spec.kindMin) / spec.kindStep).round() : null;
    final fmt = stepped && spec.kindStep >= 1.0
        ? value.toStringAsFixed(0)
        : value.toStringAsFixed(2);
    return Row(
      children: [
        SizedBox(
          width: 56,
          child: Text(spec.label,
              style: const TextStyle(color: Colors.white54, fontSize: 10)),
        ),
        Expanded(
          child: Slider(
            value: value,
            min: spec.kindMin,
            max: spec.kindMax,
            divisions: divisions,
            onChanged: (v) {
              setState(() => _params[idx] = v);
              _engine.setFxParam(widget.deck.deck, widget.slot, idx, v);
            },
          ),
        ),
        SizedBox(
          width: 64,
          child: Text(
            spec.unit.isNotEmpty ? '$fmt ${spec.unit}' : fmt,
            textAlign: TextAlign.right,
            style: const TextStyle(color: Colors.white54, fontSize: 10,
                fontFeatures: [FontFeature.tabularFigures()]),
          ),
        ),
      ],
    );
  }
}
