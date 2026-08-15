//! 混音台面板（主屏中心列）：两侧旋钮列（gain→high→mid→low→filter 纵向）
//! + 中心音量推子区（两轨 VU 在推子中间）+ 推子区下方共享交叉推子。
//!
//! 旋钮：GAIN/FILTER 总线初始化、EQ 三带快照绑定；双击回 0（MixerKnob）。
//! 音量推子双击回默认 1.0；交叉推子双击回中 0，宽度对齐音量推子区
//! （不占用两侧 EQ 位置）。XFADE：本地状态 + 总线初始化（−1..+1）。

import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import '../engine/engine_controller.dart';
import '../src/rust/api.dart';
import 'mixer_knob.dart';

/// 三带 EQ 常量（与引擎 MIN/MAX_GAIN_DB 对齐，原 eq_panel.dart 迁移）。
const double kEqMinDb = -40.0;
const double kEqMaxDb = 6.0;

/// 音量推子双击回正目标（单位增益；与 DeckController 初始值一致）。
const double kDefaultVolume = 1.0;

class MixerPanel extends StatefulWidget {
  const MixerPanel({super.key, this.width = 300});

  final double width;

  @override
  State<MixerPanel> createState() => _MixerPanelState();
}

class _MixerPanelState extends State<MixerPanel> {
  double _xfade = 0;

  @override
  void initState() {
    super.initState();
    // 总线初始化：桥缺失（测试）→ 0
    try {
      _xfade = busGet(path: 'Master.crossfader').clamp(-1.0, 1.0);
    } catch (_) {}
  }

  @override
  Widget build(BuildContext context) {
    final engine = EngineController.instance;
    return SizedBox(
      width: widget.width,
      child: Container(
        margin: const EdgeInsets.all(6),
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
        decoration: BoxDecoration(
          color: const Color(0xFF23282F),
          borderRadius: BorderRadius.circular(6),
        ),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            _KnobColumn(deck: 0, dc: engine.decks[0]),
            const SizedBox(width: 6),
            Expanded(child: _faderSection(engine)),
            const SizedBox(width: 6),
            _KnobColumn(deck: 1, dc: engine.decks[1]),
          ],
        ),
      ),
    );
  }

  /// 中心推子区：Row[推子0 | 6 | VU0 | 6 | VU1 | 6 | 推子1]
  /// （两轨 VU 在推子中间），下方 XFADE 宽度对齐推子区。
  Widget _faderSection(EngineController engine) {
    return Row(
      children: [
        Expanded(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Expanded(
                child: Row(
                  children: [
                    Expanded(child: _volumeFader(engine, 0)),
                    const SizedBox(width: 6),
                    _VuBar(engine.decks[0]),
                    const SizedBox(width: 6),
                    _VuBar(engine.decks[1]),
                    const SizedBox(width: 6),
                    Expanded(child: _volumeFader(engine, 1)),
                  ],
                ),
              ),
              const SizedBox(height: 8),
              _xfadeRow(engine),
            ],
          ),
        ),
      ],
    );
  }

  /// 垂直音量推子（RotatedBox 转 -90°，min 在下）；双击回默认音量。
  Widget _volumeFader(EngineController engine, int deck) {
    final dc = engine.decks[deck];
    return ValueListenableBuilder<double>(
      valueListenable: dc.volume,
      builder: (_, vol, _) {
        return GestureDetector(
          behavior: HitTestBehavior.opaque,
          onDoubleTap: () => engine.setVolume(deck, kDefaultVolume),
          child: SliderTheme(
            data: SliderTheme.of(context).copyWith(
              activeTrackColor:
                  const Color(0xFF3949AB).withValues(alpha: 0.6),
              inactiveTrackColor: const Color(0xFF2E353D),
              thumbColor: const Color(0xFF3949AB),
              overlayShape: const RoundSliderOverlayShape(overlayRadius: 14),
            ),
            child: RotatedBox(
              quarterTurns: 3,
              child: Slider(
                value: vol.clamp(0.0, 1.0),
                onChanged: (v) => engine.setVolume(dc.deck, v),
              ),
            ),
          ),
        );
      },
    );
  }

  /// 交叉推子（宽度与音量推子区对齐）：双击回中 0。
  Widget _xfadeRow(EngineController engine) {
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onDoubleTap: () {
        setState(() => _xfade = 0);
        engine.setCrossfader(0);
      },
      child: Stack(
        alignment: Alignment.center,
        children: [
          Slider(
            value: _xfade,
            min: -1,
            max: 1,
            onChanged: (v) {
              setState(() => _xfade = v);
              engine.setCrossfader(v);
            },
          ),
          // 中心刻度线（0 = 居中）
          IgnorePointer(
            child: Container(
              width: 2,
              height: 14,
              color: Colors.white.withValues(alpha: 0.4),
            ),
          ),
        ],
      ),
    );
  }
}

/// 单通道旋钮列：DECK 标识 + GAIN/HIGH/MID/LOW/FILTER 纵向排布（统一 44px）。
class _KnobColumn extends StatelessWidget {
  const _KnobColumn({required this.deck, required this.dc});

  final int deck;
  final DeckController dc;

  @override
  Widget build(BuildContext context) {
    final engine = EngineController.instance;
    return Column(
      children: [
        Text(
          'DECK ${deck + 1}',
          style: const TextStyle(color: Colors.white38, fontSize: 10),
        ),
        const SizedBox(height: 8),
        MixerKnob(
          label: 'GAIN',
          min: -12,
          max: 12,
          initFromBus: 'Deck${deck + 1}.gain',
          format: (v) => '${v > 0 ? '+' : ''}${v.toStringAsFixed(1)} dB',
          onChanged: (v) => engine.setGain(deck, v),
        ),
        const SizedBox(height: 8),
        MixerKnob(
          label: 'HIGH',
          min: kEqMinDb,
          max: kEqMaxDb,
          color: const Color(0xFF1E88E5),
          value: dc.eqHigh,
          onChanged: (v) => engine.setEq(deck, 2, v),
        ),
        const SizedBox(height: 8),
        MixerKnob(
          label: 'MID',
          min: kEqMinDb,
          max: kEqMaxDb,
          color: const Color(0xFF43A047),
          value: dc.eqMid,
          onChanged: (v) => engine.setEq(deck, 1, v),
        ),
        const SizedBox(height: 8),
        MixerKnob(
          label: 'LOW',
          min: kEqMinDb,
          max: kEqMaxDb,
          color: const Color(0xFFE53935),
          value: dc.eqLow,
          onChanged: (v) => engine.setEq(deck, 0, v),
        ),
        const SizedBox(height: 8),
        MixerKnob(
          label: 'FILTER',
          min: -1,
          max: 1,
          color: const Color(0xFFAB47BC),
          initFromBus: 'Deck${deck + 1}.filter',
          onChanged: (v) => engine.setFilter(deck, v),
        ),
      ],
    );
  }
}

/// 垂直电平指示：配色同 master 条 VU，quarterTurns:3 使填充自下而上。
class _VuBar extends StatelessWidget {
  const _VuBar(this.dc);

  final DeckController dc;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      width: 8,
      child: RotatedBox(
        quarterTurns: 3,
        child: ValueListenableBuilder<double>(
          valueListenable: dc.vu,
          builder: (_, v, _) => ClipRRect(
            borderRadius: BorderRadius.circular(2),
            child: LinearProgressIndicator(
              value: v.clamp(0.0, 1.0),
              minHeight: 6,
              backgroundColor: const Color(0xFF2E353D),
              valueColor: const AlwaysStoppedAnimation(Color(0xFF43A047)),
            ),
          ),
        ),
      ),
    );
  }
}
