//! 引擎单例：60Hz 快照 tick + 载曲/seek/控制总线入口。
//!
//! D7：单个 Timer.periodic(16ms) → 一次 `snapshotAll()` 同步调用 →
//! 分发给两个 DeckController 的细粒度 notifier。桥未初始化（测试环境
//! / .so 缺失）时不启动 tick，避免悬空 Timer。

import 'dart:async';
import 'dart:io';

import 'package:flutter/foundation.dart';

import '../src/rust/api.dart';
import 'deck_controller.dart';
import 'wave_display_mode.dart';

class EngineController {
  EngineController._();

  static final EngineController instance = EngineController._();

  final decks = [DeckController(0), DeckController(1)];
  final error = ValueNotifier<String?>(null);

  /// master zoom（窗口 60/zoom 秒），两 deck 共享。
  final zoom = ValueNotifier<double>(1.0);

  /// 滚动波形显示模式（默认 rgb = Slint 风格；settings 落地前用 master 条按钮切换）。
  final waveMode = ValueNotifier<WaveDisplayMode>(WaveDisplayMode.rgb);
  final masterVolume = ValueNotifier<double>(0.8);
  final masterVu = ValueNotifier<double>(0);

  /// FX 效果清单（manifest，启动时取一次缓存）。测试环境为空列表。
  List<FxEffectWire> fxManifestsCache = const [];

  Timer? _timer;
  bool _running = false;
  bool _engineOk = false;

  bool get engineOk => _engineOk;

  /// 启动引擎 + 60Hz tick。桥不可用（测试环境）时只设 error、不启动 Timer。
  void start() {
    if (_running) return;
    _running = true;
    try {
      initEngine();
      _engineOk = true;
      fxManifestsCache = fxManifests();
      busSet(path: 'Master.volume', value: masterVolume.value);
      // 总线控制点初值 0.0：deck 音量不写会全静音（引擎输出 ×0 → VU=0）
      for (final dc in decks) {
        busSet(path: 'Deck${dc.deck + 1}.volume', value: dc.volume.value);
        // P10.1：量化默认开（seek 吸附最近拍；无网格引擎自动退化恒等）
        busSet(path: 'Deck${dc.deck + 1}.quantize', value: 1);
      }
    } catch (e) {
      error.value = '引擎启动失败: $e';
      return;
    }
    final track = Platform.environment['HYPERMIXX_TRACK'];
    if (track != null && track.isNotEmpty) {
      loadTrackInto(0, track);
    }
    _timer = Timer.periodic(const Duration(milliseconds: 16), (_) => _tick());
  }

  void stop() {
    _timer?.cancel();
    _timer = null;
    _running = false;
  }

  void _tick() {
    final s = snapshotAll();
    decks[0].updateFromWire(s.deck0);
    decks[1].updateFromWire(s.deck1);
    masterVolume.value = s.master.volume;
    masterVu.value = s.master.vu;
  }

  /// 载曲：起分析流 + 读元数据（封面/title）。重复调用 = 重载。
  void loadTrackInto(int deck, String path) {
    final dc = decks[deck];
    dc.resetCueState();
    dc.attachAnalysis(loadTrack(deck: deck, path: path));
    readMetadata(path: path)
        .then((m) {
          dc.title = m.title;
          dc.artist = m.artist;
          dc.cover = m.cover;
          dc.coverMime = m.coverMime;
          dc.metaRev.value++;
        })
        .catchError((Object e) {
          debugPrint('元数据读取失败: $e');
        });
  }

  void seekTo(int deck, double seconds) {
    seek(deck: deck, seconds: seconds);
  }

  /// 精确跳转（不量化；cue/hotcue 召回用——量化会把召回到点吸到邻近拍点）。
  void seekExactTo(int deck, double seconds) {
    seekExact(deck: deck, seconds: seconds);
  }

  /// 按拍跳跃（简单加减，拍长匹配当前速度；beats 可负）。
  void beatJump(int deck, double beats) {
    beatjump(deck: deck, beats: beats);
  }

  /// 激活 beat loop（拍数，量化起止）。取消由 UI 写 loop_active=0。
  void activateBeatLoop(int deck, double beats) {
    setBeatLoop(deck: deck, beats: beats);
  }

  void setLoopActive(int deck, bool on) {
    busSet(path: 'Deck${deck + 1}.loop_active', value: on ? 1 : 0);
  }

  /// P18 ManualLoop：loop 边界（秒，原始位置——不经量化；引擎总线
  /// 边沿检测进捕获，见 deck.rs update_params）。配合 setLoopActive 用。
  void setLoopIn(int deck, double seconds) {
    busSet(path: 'Deck${deck + 1}.loop_in', value: seconds);
  }

  void setLoopOut(int deck, double seconds) {
    busSet(path: 'Deck${deck + 1}.loop_out', value: seconds);
  }

  void setVolume(int deck, double v) {
    busSet(path: 'Deck${deck + 1}.volume', value: v);
  }

  /// 通道增益（dB，-12..+12）。
  void setGain(int deck, double db) {
    busSet(path: 'Deck${deck + 1}.gain', value: db);
  }

  /// deck 滤波旋钮（-1..+1；正=LP，负=HP，0=旁路）。
  void setFilter(int deck, double v) {
    busSet(path: 'Deck${deck + 1}.filter', value: v);
  }

  /// 交叉推子（-1..+1，0 居中）。
  void setCrossfader(double v) {
    busSet(path: 'Master.crossfader', value: v);
  }

  /// 对拍临时加减速（-1/0/+1，按住期间生效）。
  void setNudge(int deck, int v) {
    busSet(path: 'Deck${deck + 1}.nudge', value: v.toDouble());
  }

  void toggleWaveMode() {
    waveMode.value = waveMode.value == WaveDisplayMode.rgb
        ? WaveDisplayMode.bands
        : WaveDisplayMode.rgb;
  }

  void setMasterVolume(double v) {
    masterVolume.value = v;
    busSet(path: 'Master.volume', value: v);
  }

  void setRate(int deck, double percent) {
    busSet(path: 'Deck${deck + 1}.rate', value: percent);
  }

  void setPlaying(int deck, bool playing) {
    busSet(path: 'Deck${deck + 1}.play', value: playing ? 1 : 0);
  }

  void setSync(int deck, bool on) {
    busSet(path: 'Deck${deck + 1}.sync', value: on ? 1 : 0);
  }

  void setKeylock(int deck, bool on) {
    busSet(path: 'Deck${deck + 1}.keylock', value: on ? 1 : 0);
  }

  /// EQ 三带增益（dB，-40..+6）。band: 0=low 1=mid 2=high。
  static const _eqPaths = ['eq_low', 'eq_mid', 'eq_high'];
  void setEq(int deck, int band, double db) {
    busSet(path: 'Deck${deck + 1}.${_eqPaths[band]}', value: db);
  }

  /// FX 槽效果类型（0 = 无；1..8 = manifest id）。
  void setFxType(int deck, int slot, int fxId) {
    busSet(path: 'Deck${deck + 1}.fx${slot + 1}_type', value: fxId.toDouble());
  }

  void setFxEnable(int deck, int slot, bool on) {
    busSet(path: 'Deck${deck + 1}.fx${slot + 1}_enable', value: on ? 1 : 0);
  }

  void setFxDrywet(int deck, int slot, double drywet) {
    busSet(path: 'Deck${deck + 1}.fx${slot + 1}_drywet', value: drywet);
  }

  /// FX 参数（p1..p4，自然单位；manifest 位对应）。
  void setFxParam(int deck, int slot, int paramIdx, double v) {
    busSet(path: 'Deck${deck + 1}.fx${slot + 1}_p${paramIdx + 1}', value: v);
  }
}

/// 便捷入口。
EngineController engine() => EngineController.instance;
