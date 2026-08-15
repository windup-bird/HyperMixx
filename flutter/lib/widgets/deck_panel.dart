//! Deck 面板（左右 deck 列）：四段纵向布局。
//! 行1：封面（点击载曲）+ title/artist（左）；时间、key/bpm、tempo（右，
//! 同排）、keylock（最右；P19 起 sync 移入 transport 行）。
//! 行2：全区半波预览（OverviewWave，h64）。
//! 行3（P18 重构，P21 调整）：loop/jump（预览下、pad 上，宽度平分）+
//! 打击垫区 + DeckFx（pad 右 tempo 左）；TempoFader 跨整列高。
//! P22.3：pads:fx 固定 1:1（用户要求，删除 P22.2 阈值切换）；fx 内
//! beat/select 两行按钮横向铺满 fx 列宽。
//! 行4（P19 transport 行）：6 等分 SHIFT/SYNC/CUE/播放/<< />>（见
//! transport_row.dart）。
//! 音量/增益/EQ/滤波/交叉推子在中心 MixerPanel，变速在 Tempofader。

import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../engine/deck_controller.dart';
import '../engine/engine_controller.dart';
import '../src/rust/api.dart';
import 'beatjump_panel.dart';
import 'deck_fx.dart';
import 'deck_pads.dart';
import 'manual_loop.dart';
import 'overview_wave.dart';
import 'tempo_fader.dart';
import 'transport_row.dart';

class DeckPanel extends StatefulWidget {
  const DeckPanel({super.key, required this.deck});

  final DeckController deck;

  @override
  State<DeckPanel> createState() => _DeckPanelState();
}

// P18.1/P20 高度常量（tempo 列跨整列）：
// loop/jump 66（30×2+6）、pads 116（选项卡 26+4+pad 40+6+pad 40）、
// DeckFx 116 = kPadsRowHeight（P20 放大填满行高）、transport 30。
const double kLoopJumpPanelHeight = 66;
const double kDeckFxHeight = kPadsRowHeight;
const double kPadsRowHeight = 26 + 4 + 40 + 6 + 40;
const double kTransportRowHeight = 30;
/// TempoFader 高度 = loop/jump + pad 行 + transport + 间距。
const double kTempoFaderHeight =
    kLoopJumpPanelHeight + 6 + kPadsRowHeight + 6 + kTransportRowHeight;

class _DeckPanelState extends State<DeckPanel> {
  /// 点击封面载曲（P18：移除载曲输入控件）→ 系统文件对话框
  /// （桥 rfd/XDG portal；#[frb(sync)]：阻塞 UI 直到选择/取消）。
  void _loadPressed() {
    final engine = EngineController.instance;
    final picked = pickAudioFile();
    if (picked == null || picked.isEmpty) return;
    if (!File(picked).existsSync()) {
      _snack('文件不存在: $picked');
      return;
    }
    engine.loadTrackInto(widget.deck.deck, picked);
  }

  void _snack(String msg) {
    ScaffoldMessenger.of(context)
      ..hideCurrentSnackBar()
      ..showSnackBar(
        SnackBar(
          content: Text(msg, style: const TextStyle(fontSize: 12)),
          duration: const Duration(seconds: 3),
        ),
      );
  }

  @override
  Widget build(BuildContext context) {
    final dc = widget.deck;
    final engine = EngineController.instance;
    return Container(
      margin: const EdgeInsets.all(6),
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 8),
      decoration: BoxDecoration(
        color: const Color(0xFF23282F),
        borderRadius: BorderRadius.circular(6),
      ),
      // 小窗口下面板高度不足时滚动，杜绝 RenderFlex overflow
      child: SingleChildScrollView(
        child: Column(
          children: [
            _row1(context, dc, engine),
            const SizedBox(height: 6),
            // deckinfo 下方：全区半波预览
            SizedBox(height: 64, child: OverviewWave(deck: dc)),
            const SizedBox(height: 6),
            // P18：loop/jump（预览下、pad 上，宽度平分）+ pad + DeckFx +
            // transport 三行左侧；TempoFader 跨整列（固定高 = 三行总高；
            // 不可用 IntrinsicHeight——子树内嵌 Expanded/Spacer 不支持
            // intrinsic 计算）。
            Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Expanded(
                  child: Column(
                    children: [
                      Row(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Expanded(child: ManualLoop(deck: dc)),
                          const SizedBox(width: 6),
                          Expanded(child: BeatJumpPanel(deck: dc)),
                        ],
                      ),
                      const SizedBox(height: 6),
                      // P22.3：pads:fx = 1:1 固定（用户要求）。窄窗下
                      // DeckFx 左列 Flexible+FittedBox 等比缩小（P20.1），
                      // pads 文字 FittedBox（P18.1）。
                      Row(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Expanded(child: DeckPads(deck: dc)),
                          const SizedBox(width: 2),
                          Expanded(
                            child: SizedBox(
                              height: kDeckFxHeight,
                              child: DeckFx(deck: dc),
                            ),
                          ),
                        ],
                      ),
                      const SizedBox(height: 6),
                      _transportRow(context, dc, engine),
                    ],
                  ),
                ),
                const SizedBox(width: 8),
                SizedBox(
                  width: 72,
                  height: kTempoFaderHeight,
                  child: TempoFader(deck: dc),
                ),
              ],
            ),
          ],
        ),
      ),
    );
  }

  // ---- 行1：元数据 + 状态 ----
  Widget _row1(
    BuildContext context,
    DeckController dc,
    EngineController engine,
  ) {
    return SizedBox(
      height: 68,
      child: Row(
        children: [
          _cover(dc),
          const SizedBox(width: 8),
          Expanded(child: _titleArtist(dc)),
          const SizedBox(width: 8),
          // 窄窗口下 info 列等比缩入，不溢出
          Expanded(
            child: FittedBox(
              fit: BoxFit.scaleDown,
              alignment: Alignment.centerRight,
              child: _infoColumn(dc, engine),
            ),
          ),
        ],
      ),
    );
  }

  /// 封面 = 载曲入口（P18：点击打开文件对话框；hover 显示载曲提示）。
  Widget _cover(DeckController dc) {
    return GestureDetector(
      onTap: _loadPressed,
      child: Tooltip(
        message: '点击载曲',
        child: ValueListenableBuilder<int>(
          valueListenable: dc.metaRev,
          builder: (context, _, _) {
            final Uint8List? bytes = dc.cover;
            if (bytes == null || bytes.isEmpty) {
              return Container(
                width: 64,
                height: 64,
                color: const Color(0xFF2E353D),
                child: const Icon(
                  Icons.music_note,
                  color: Colors.white24,
                  size: 28,
                ),
              );
            }
            return ClipRRect(
              borderRadius: BorderRadius.circular(4),
              child: Image.memory(
                bytes,
                width: 64,
                height: 64,
                fit: BoxFit.cover,
                cacheWidth: 128,
              ),
            );
          },
        ),
      ),
    );
  }

  Widget _titleArtist(DeckController dc) {
    return ValueListenableBuilder<int>(
      valueListenable: dc.metaRev,
      builder: (context, _, _) {
        return Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            Text(
              dc.title ?? '点击左侧载入',
              style: const TextStyle(
                color: Colors.white,
                fontSize: 16,
                fontWeight: FontWeight.w600,
              ),
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
            ),
            const SizedBox(height: 2),
            Text(
              dc.artist ?? (dc.analysisError ?? ''),
              style: TextStyle(
                color: Colors.white.withValues(alpha: 0.55),
                fontSize: 12,
              ),
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
            ),
          ],
        );
      },
    );
  }

  Widget _infoColumn(DeckController dc, EngineController engine) {
    // P19：sync 移入 transport 行（TransportRow），信息列只留 KEY。
    return Column(
      mainAxisSize: MainAxisSize.min,
      mainAxisAlignment: MainAxisAlignment.center,
      crossAxisAlignment: CrossAxisAlignment.end,
      children: [
        Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            ValueListenableBuilder<String>(
              valueListenable: dc.timeText,
              builder: (_, v, _) => Text(
                v,
                style: const TextStyle(
                  color: Colors.white,
                  fontSize: 13,
                  fontFeatures: [FontFeature.tabularFigures()],
                ),
              ),
            ),
            const SizedBox(width: 6),
            ValueListenableBuilder<String>(
              valueListenable: dc.bpmKeyText,
              builder: (_, v, _) => Text(
                v,
                style: const TextStyle(
                  color: Color(0xFF9CCC65),
                  fontSize: 13,
                ),
              ),
            ),
            const SizedBox(width: 6),
            ValueListenableBuilder<String>(
              valueListenable: dc.tempoText,
              builder: (_, v, _) => Text(
                v,
                textAlign: TextAlign.right,
                style: const TextStyle(
                  color: Colors.orangeAccent,
                  fontSize: 11,
                  height: 1.2,
                ),
              ),
            ),
            const SizedBox(width: 6),
            _toggle(
              dc: dc,
              notifier: dc.keylockOn,
              label: 'KEY',
              onChanged: (v) => engine.setKeylock(dc.deck, v),
            ),
          ],
        ),
      ],
    );
  }

  Widget _toggle({
    required DeckController dc,
    required ValueListenable<bool> notifier,
    required String label,
    required void Function(bool) onChanged,
  }) {
    return ValueListenableBuilder<bool>(
      valueListenable: notifier,
      builder: (_, on, _) {
        return TextButton(
          onPressed: () => onChanged(!on),
          style: TextButton.styleFrom(
            padding: const EdgeInsets.symmetric(horizontal: 8),
            minimumSize: const Size(48, 28),
            backgroundColor: on ? const Color(0xFF3949AB) : Colors.transparent,
            foregroundColor: on ? Colors.white : Colors.white38,
          ),
          child: Text(
            label,
            style: const TextStyle(fontSize: 11, fontWeight: FontWeight.bold),
          ),
        );
      },
    );
  }

  // ---- 行4：transport（P19 6 等分按钮，见 transport_row.dart）----
  Widget _transportRow(
    BuildContext context,
    DeckController dc,
    EngineController engine,
  ) {
    return TransportRow(deck: dc);
  }
}
