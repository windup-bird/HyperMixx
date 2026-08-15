//! P8 打击垫区：主 CUE 按钮 + 2×4 pad 网格 + 4 模式选项卡 + 2 翻页按钮。
//!
//! 模式：
//! - HOTCUE：16 槽，逻辑与主 cue 相同（播放召回 / 停播落点 / 位于点位按住试听
//!   松开回点），右键或角上 × 删除；
//! - LOOP：1/32..64 拍，点击激活/取消（激活当前拍数 → 再点取消）；
//! - BEATJUMP：P21 成对横排（◀1 ▶1 ◀2 ▶2 ...，左跳右跳相邻），窗口 8
//!   步长 4 → 页0 = 1/2/4/8、页1 = 4/8/16/32，简单加减、匹配当前速度；
//! - FX：P18.1 起只占位（功能并入 DeckFx 单通道，每 deck 一个 fx1）——
//!   8 个 pad 显示 'FX' 灰死不可点。
//!
//! 窗口 8 pad、翻页步长 4：hotcue 3 页（0/4/8）、loop/beatjump 2 页
//! （0/4 滚动）、fx 无翻页。
//!
//! 全部动作经 `PadActions` 出口（默认走 EngineController/桥），
//! widget 测试注入假实现即可脱离桥。

import 'package:flutter/material.dart';

import '../engine/cue_action.dart';
import '../engine/deck_controller.dart';
import '../engine/engine_controller.dart';

/// 可注入动作出口。默认直连 EngineController/桥；测试子类化记录调用。
class PadActions {
  const PadActions();

  void seekExactTo(int deck, double seconds) =>
      engine().seekExactTo(deck, seconds);
  void setPlaying(int deck, bool on) => engine().setPlaying(deck, on);
  void setLoopActive(int deck, bool on) => engine().setLoopActive(deck, on);
  /// P18 ManualLoop：loop 边界（秒，原始位置不经量化；引擎边沿检测进捕获）。
  void setLoopIn(int deck, double seconds) => engine().setLoopIn(deck, seconds);
  void setLoopOut(int deck, double seconds) =>
      engine().setLoopOut(deck, seconds);
  void activateBeatLoop(int deck, double beats) =>
      engine().activateBeatLoop(deck, beats);
  void beatJump(int deck, double beats) => engine().beatJump(deck, beats);
  void setFxEnable(int deck, int slot, bool on) =>
      engine().setFxEnable(deck, slot, on);
  void setFxType(int deck, int slot, int fxId) =>
      engine().setFxType(deck, slot, fxId);
  void setFxDrywet(int deck, int slot, double v) =>
      engine().setFxDrywet(deck, slot, v);
  void setFxParam(int deck, int slot, int paramIdx, double v) =>
      engine().setFxParam(deck, slot, paramIdx, v);
  /// P19 transport 行：sync 切换（点击开/关）。
  void setSync(int deck, bool on) => engine().setSync(deck, on);
  /// P19 transport 行：nudge 按住值（松开写 0，引擎侧保持）。
  void setNudge(int deck, int v) => engine().setNudge(deck, v);
}

/// 主 CUE 按钮（transport 行）：按住/松开语义。
/// 按下：播放 → P19 起暂停并回 cue 点（原：召回继续播）；停播不在点位
/// → 落点；停播位于点位 → 试听。
/// 松开：仅试听有动作（停播 + 回 cue 点）。
/// `width`：null = 由父约束撑满（P19 6 等分），默认 46（独立使用）。
class CueButton extends StatefulWidget {
  const CueButton({
    super.key,
    required this.deck,
    this.actions = const PadActions(),
    this.width,
  });

  final DeckController deck;
  final PadActions actions;
  final double? width;

  @override
  State<CueButton> createState() => _CueButtonState();
}

class _CueButtonState extends State<CueButton> {
  CuePressResult? _press;
  bool _held = false;

  void _down() {
    final dc = widget.deck;
    final res = nextCueAction(
      playing: dc.playing.value,
      playhead: dc.playhead.value,
      point: dc.cuePoint.value,
    );
    _press = res;
    setState(() => _held = true);
    switch (res.kind) {
      case CuePressKind.recall:
        widget.actions.seekExactTo(dc.deck, res.point!);
        // P19：播放时点击 CUE = 暂停并回到 cue 点（hotcue 保持继续播召回）
        widget.actions.setPlaying(dc.deck, false);
      case CuePressKind.set:
        dc.cuePoint.value = res.cueToSet;
      case CuePressKind.preview:
        widget.actions.setPlaying(dc.deck, true);
    }
  }

  void _up() {
    final res = _press;
    _press = null;
    setState(() => _held = false);
    if (res == null) return;
    final seek = cueReleaseSeek(res);
    if (seek != null) {
      widget.actions.setPlaying(widget.deck.deck, false);
      widget.actions.seekExactTo(widget.deck.deck, seek);
    }
  }

  @override
  Widget build(BuildContext context) {
    final dc = widget.deck;
    return ListenableBuilder(
      listenable: Listenable.merge([dc.cuePoint, dc.playing, dc.playhead]),
      builder: (_, _) {
        final cue = dc.cuePoint.value;
        // 停播且指针位于 cue 点 = "armed"（按下即试听）
        final armed = !dc.playing.value &&
            cue != null &&
            (dc.playhead.value - cue).abs() <= kCueEpsilonSecs;
        return GestureDetector(
          behavior: HitTestBehavior.opaque,
          onTapDown: (_) => _down(),
          onTapUp: (_) => _up(),
          onTapCancel: _up,
          child: Container(
            width: widget.width,
            height: 30,
            decoration: BoxDecoration(
              color: armed
                  ? const Color(0xFFFF7043)
                  : (_held ? const Color(0x66FF7043) : Colors.transparent),
              borderRadius: BorderRadius.circular(4),
              border: Border.all(
                color: cue != null ? const Color(0xFFFF7043) : Colors.white24,
              ),
            ),
            alignment: Alignment.center,
            child: Text(
              'CUE',
              style: TextStyle(
                fontSize: 11,
                fontWeight: FontWeight.bold,
                color: armed || cue != null ? Colors.white : Colors.white38,
              ),
            ),
          ),
        );
      },
    );
  }
}

/// 2×4 打击垫区（deckinfo 下方）。
class DeckPads extends StatefulWidget {
  const DeckPads({super.key, required this.deck, this.actions = const PadActions()});

  final DeckController deck;
  final PadActions actions;

  @override
  State<DeckPads> createState() => _DeckPadsState();
}

class _DeckPadsState extends State<DeckPads> {
  static const _modeTabs = ['HOTCUE', 'LOOP', 'BEATJUMP', 'FX'];
  static const _windowSize = 8;
  static const _step = 4;

  /// loop 拍数长列表（1/32..64，12 项，窗口 8 步长 4 → 2 页）。
  static const _loopBeats = [
    1 / 32, 1 / 16, 1 / 8, 1 / 4, 1 / 2, 1.0, 2.0, 4.0, //
    8.0, 16.0, 32.0, 64.0,
  ];
  static const _loopLabels = [
    '1/32', '1/16', '1/8', '1/4', '1/2', '1', '2', '4', //
    '8', '16', '32', '64',
  ];

  /// P21 beatjump 线性列表（成对横排：◀1 ▶1 ◀2 ▶2 ...——左跳右跳相邻，
  /// 不是旧的上排全左跳/下排全右跳），窗口 8 步长 4 → 页0 = 左右 1/2/4/8、
  /// 页1 = 滚动 4 项（4/8/16/32），与 LOOP 列表同机制。
  static const _beatjumpBeats = [
    -1.0, 1.0, -2.0, 2.0, -4.0, 4.0, -8.0, 8.0, //
    -16.0, 16.0, -32.0, 32.0,
  ];

  int _mode = 0;
  final List<int> _page = [0, 0, 0, 0];
  /// 当前按住的网格位（0..7，按压高亮）。
  int? _pressed;
  /// hotcue 试听中待执行的松开动作。
  CuePressResult? _pendingCue;

  int get _itemCount => switch (_mode) {
        0 => 16, // hotcue 1..16
        1 => _loopBeats.length,
        2 => _beatjumpBeats.length, // P21：成对横排列表（12 项）
        _ => 8, // fx 槽
      };

  int get _pageCount {
    // P21 beatjump 同走窗口/步长（12 项窗口 8 步长 4 → 2 页，滚动间隔 4）。
    final maxStart = (_itemCount - _windowSize).clamp(0, 1 << 30);
    return maxStart ~/ _step + 1;
  }

  int get _windowStart {
    final maxStart = (_itemCount - _windowSize).clamp(0, 1 << 30);
    return (_page[_mode] * _step).clamp(0, maxStart);
  }

  void _setPage(int delta) {
    setState(() {
      _page[_mode] = (_page[_mode] + delta).clamp(0, _pageCount - 1);
    });
  }

  // ---- hotcue ----
  void _hotcueDown(int slot, double? point, int gridIndex) {
    setState(() => _pressed = gridIndex);
    final dc = widget.deck;
    final res = nextCueAction(
      playing: dc.playing.value,
      playhead: dc.playhead.value,
      point: point,
    );
    _pendingCue = res;
    switch (res.kind) {
      case CuePressKind.recall:
        widget.actions.seekExactTo(dc.deck, res.point!);
      case CuePressKind.set:
        dc.hotcues[slot].value = res.cueToSet;
      case CuePressKind.preview:
        widget.actions.setPlaying(dc.deck, true);
    }
  }

  void _hotcueUp() {
    final res = _pendingCue;
    _pendingCue = null;
    setState(() => _pressed = null);
    if (res == null) return;
    final seek = cueReleaseSeek(res);
    if (seek != null) {
      widget.actions.setPlaying(widget.deck.deck, false);
      widget.actions.seekExactTo(widget.deck.deck, seek);
    }
  }

  Widget _hotcuePad(int slot) {
    final dc = widget.deck;
    final gridIndex = slot - _windowStart;
    return ValueListenableBuilder<double?>(
      valueListenable: dc.hotcues[slot],
      builder: (_, point, _) {
        final set = point != null;
        return GestureDetector(
          behavior: HitTestBehavior.opaque,
          onTapDown: (_) => _hotcueDown(slot, point, gridIndex),
          onTapUp: (_) => _hotcueUp(),
          onTapCancel: _hotcueUp,
          onSecondaryTap: () => dc.hotcues[slot].value = null,
          child: _padBox(
            label: '${slot + 1}',
            lit: set,
            litColor: const Color(0xFFE65100),
            pressed: _pressed == gridIndex,
            child: set
                ? Positioned(
                    top: 1,
                    right: 2,
                    child: GestureDetector(
                      onTap: () => dc.hotcues[slot].value = null,
                      child: const Icon(Icons.close, size: 10, color: Colors.white70),
                    ),
                  )
                : null,
          ),
        );
      },
    );
  }

  // ---- loop ----
  Widget _loopPad(double beats, String label) {
    final dc = widget.deck;
    return ValueListenableBuilder<bool>(
      valueListenable: dc.loopActive,
      builder: (_, active, _) {
        // 匹配当前激活环的拍数（loopOut−loopIn 按有效 BPM 折算）
        final cur = active
            ? (dc.loopOut.value - dc.loopIn.value) * dc.bpm.value / 60.0
            : 0.0;
        final isActive = active && (cur - beats).abs() < 0.05;
        return GestureDetector(
          behavior: HitTestBehavior.opaque,
          // onTapDown 按下即激活（P22-D）：loop 激活瞬间的播放头位置决定
          // 捕获起点，onTap 要等手势仲裁（~数十毫秒）才触发——晚了首圈
          // 就不完整（P22 卡顿源 2），同 beatjump P12 先例。
          onTapDown: (_) {
            if (isActive) {
              widget.actions.setLoopActive(dc.deck, false);
            } else {
              widget.actions.activateBeatLoop(dc.deck, beats);
            }
          },
          child: _padBox(
            label: label,
            lit: isActive,
            litColor: const Color(0xFF2E7D32),
          ),
        );
      },
    );
  }

  // ---- beatjump ----
  /// 整拍标签不带小数：1.0 → '1'。
  String _beatLabel(double b) =>
      b == b.roundToDouble() ? b.toInt().toString() : b.toString();

  Widget _beatjumpPad(double? beats) {
    final dc = widget.deck;
    if (beats == null) {
      return _padBox(label: '', lit: false, litColor: Colors.transparent, dead: true);
    }
    final b = beats.abs();
    final label = beats < 0 ? '◀${_beatLabel(b)}' : '▶${_beatLabel(b)}';
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      // onTapDown 按下即跳（P12）：beatjump 对时序敏感，onTap 要等手势
      // 仲裁（抬指 + 竞争判定 ~数十毫秒）才触发——跳晚了相位就丢了。
      onTapDown: (_) => widget.actions.beatJump(dc.deck, beats),
      child: _padBox(label: label, lit: false, litColor: const Color(0xFF283593)),
    );
  }

  // ---- fx ----
  // P18.1：FX 只占位（功能并入 DeckFx 单通道），无交互。
  Widget _fxPad(int slot) {
    return _padBox(
      label: 'FX',
      lit: false,
      litColor: const Color(0xFF6A1B9A),
      dead: true,
    );
  }

  // ---- 网格位 ----
  Widget _pad(int gridIndex) {
    switch (_mode) {
      case 0:
        return _hotcuePad(_windowStart + gridIndex);
      case 1:
        final w = _windowStart + gridIndex;
        return _loopPad(_loopBeats[w], _loopLabels[w]);
      case 2:
        return _beatjumpPad(_beatjumpBeats[_windowStart + gridIndex]);
      default:
        return _fxPad(gridIndex);
    }
  }

  Widget _padBox({
    required String label,
    required bool lit,
    required Color litColor,
    bool pressed = false,
    bool dead = false,
    Widget? child,
  }) {
    final Color bg;
    if (dead) {
      bg = const Color(0xFF1E232B);
    } else if (lit) {
      bg = litColor;
    } else if (pressed) {
      bg = litColor.withValues(alpha: 0.35);
    } else {
      bg = const Color(0xFF2E353D);
    }
    return Container(
      height: 40,
      margin: const EdgeInsets.symmetric(horizontal: 2),
      decoration: BoxDecoration(
        color: bg,
        borderRadius: BorderRadius.circular(4),
      ),
      // Stack 铺满整个 pad：× 定位到 pad 右上角（而不是文本右上角，
      // 后者会压住标签并把点击全吸走），文本居中。
      // P18.1 响应式：FittedBox 防窄窗 pad 文字溢出（如 '1/32'）。
      child: Stack(
        fit: StackFit.expand,
        children: [
          Center(
            child: FittedBox(
              fit: BoxFit.scaleDown,
              child: Text(
                label,
                style: TextStyle(
                  color: dead ? Colors.white12 : (lit || pressed ? Colors.white : Colors.white60),
                  fontSize: 11,
                  fontWeight: FontWeight.w600,
                ),
              ),
            ),
          ),
          ?child,
        ],
      ),
    );
  }

  Widget _modeTab(int m) {
    final selected = _mode == m;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 2),
      child: TextButton(
        onPressed: () => setState(() => _mode = m),
        style: TextButton.styleFrom(
          padding: EdgeInsets.zero,
          minimumSize: const Size(0, 22),
          tapTargetSize: MaterialTapTargetSize.shrinkWrap,
          backgroundColor: selected ? const Color(0xFF3949AB) : const Color(0xFF2E353D),
          foregroundColor: selected ? Colors.white : Colors.white38,
        ),
        child: Text(
          _modeTabs[m],
          style: const TextStyle(fontSize: 9, fontWeight: FontWeight.bold),
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    return Row(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Expanded(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              // 模式选项卡：与 4 pad 列竖向对齐（固定高 26，按钮无 48px tap 膨胀）
              SizedBox(
                height: 26,
                child: Row(
                  children: [
                    for (var m = 0; m < 4; m++) Expanded(child: _modeTab(m)),
                  ],
                ),
              ),
              const SizedBox(height: 4),
              for (var r = 0; r < 2; r++) ...[
                Row(
                  children: [
                    for (var c = 0; c < 4; c++)
                      Expanded(child: _pad(r * 4 + c)),
                  ],
                ),
                if (r == 0) const SizedBox(height: 6),
              ],
            ],
          ),
        ),
        const SizedBox(width: 2),
        // 翻页按钮：与两行 pad（h40）横向对齐（30 = 选项卡 26 + 间距 4）
        Column(
          children: [
            const SizedBox(height: 30),
            SizedBox(
              height: 40,
              child: IconButton(
                tooltip: '上一页',
                visualDensity: VisualDensity.compact,
                iconSize: 16,
                padding: EdgeInsets.zero,
                color: Colors.white54,
                onPressed: _page[_mode] > 0 ? () => _setPage(-1) : null,
                icon: const Icon(Icons.keyboard_arrow_up),
              ),
            ),
            const SizedBox(height: 6),
            SizedBox(
              height: 40,
              child: IconButton(
                tooltip: '下一页',
                visualDensity: VisualDensity.compact,
                iconSize: 16,
                padding: EdgeInsets.zero,
                color: Colors.white54,
                onPressed: _page[_mode] < _pageCount - 1 ? () => _setPage(1) : null,
                icon: const Icon(Icons.keyboard_arrow_down),
              ),
            ),
          ],
        ),
      ],
    );
  }
}
