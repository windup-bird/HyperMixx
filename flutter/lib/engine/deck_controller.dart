//! 单 deck 状态：60Hz 快照分发 + 分析事件流 + 细粒度 ValueNotifier。
//!
//! 设计（D7）：文本/开关类字段只在值变化时更新；`waveTick` 每 tick 递增
//! 驱动滚动波形重绘（painter 直接读本对象取当前值，不走 widget 重建）；
//! `waveRev` 在分析数据变化时递增（overview 仅此时重绘）。

import 'package:flutter/foundation.dart';

import '../src/rust/api.dart';
import 'wave_model.dart';

/// 分析段秒长（SEG_FRAMES / 48000 = 16s）。
const double kSegSecs = 16.0;

class DeckController extends ChangeNotifier {
  DeckController(this.deck);

  final int deck;

  // ---- 60Hz 快照字段 ----
  final timeText = ValueNotifier<String>('–:––');
  final bpmKeyText = ValueNotifier<String>('—');
  final tempoText = ValueNotifier<String>('范围 ±8%\n当前 ±0.00%');
  final playing = ValueNotifier<bool>(false);
  final syncOn = ValueNotifier<bool>(false);
  final keylockOn = ValueNotifier<bool>(false);
  final rate = ValueNotifier<double>(0);
  /// 有效速率 %（P10.1）：gridBpm>0 时 = (bpm/gridBpm − 1)×100——同步中
  /// 显示的是引擎实际速率而非滑杆位置；无网格回退滑杆 rate。
  final effRate = ValueNotifier<double>(0);
  final volume = ValueNotifier<double>(1.0);
  final vu = ValueNotifier<double>(0);
  /// EQ 三带增益（dB，-40..+6，0 = 直通）。
  final eqLow = ValueNotifier<double>(0);
  final eqMid = ValueNotifier<double>(0);
  final eqHigh = ValueNotifier<double>(0);
  final loaded = ValueNotifier<bool>(false);
  final playhead = ValueNotifier<double>(0);
  final duration = ValueNotifier<double>(0);
  /// P13 显示播放头外推状态：最近 tick 采样（引擎真值）+ 采样时刻。
  double _phSample = 0;
  DateTime? _phSampleAt;
  /// 外推速率（音轨秒/秒）：推进中 = grid 有效速率（无网格 1.0）；
  /// 停播/欠载（playhead 采样连续不变）冻结为 0。
  double _phExtrapRate = 0;
  /// 每 tick 递增：滚动波形 repaint。
  final waveTick = ValueNotifier<int>(0);
  /// 分析数据变化时递增：overview 重绘 + 波形数据刷新。
  final waveRev = ValueNotifier<int>(0);
  /// 元数据（封面/title）变化。
  final metaRev = ValueNotifier<int>(0);

  // ---- P8：cue / hotcue / loop ----
  /// 主 cue 点（秒；null = 未设）。载曲时回到曲首 0。
  final cuePoint = ValueNotifier<double?>(null);
  /// 16 个 hotcue（null = 空槽）。pad 每槽监听自己的 notifier。
  final hotcues =
      List<ValueNotifier<double?>>.generate(16, (_) => ValueNotifier<double?>(null));
  /// beat loop 状态（60Hz 快照；in/out 秒，未激活时为 0）。
  final loopActive = ValueNotifier<bool>(false);
  final loopIn = ValueNotifier<double>(0);
  final loopOut = ValueNotifier<double>(0);
  /// 有效 BPM（grid 优先，无 grid 用分析值；loop pad 匹配拍数用）。
  final bpm = ValueNotifier<double>(0);
  /// 分析网格 BPM 快照（60Hz，无网格 0；滚动波形拍轴用）。
  final gridBpm = ValueNotifier<double>(0);
  /// 变速后的实际 BPM = grid × rate（deckinfo 显示用；引擎每块写
  /// s.bpm，无网格/未播放时回退静态 bpm）。
  final effBpm = ValueNotifier<double>(0);

  // ---- 静态（载曲后一次填充）----
  String? title;
  String? artist;
  Uint8List? cover;
  String coverMime = '';
  String keyCamelot = '—';
  double trackBpm = 0;
  List<double> beats = const [];
  List<double> downbeats = const [];
  String? analysisError;

  final WaveModel wave = WaveModel();
  int _lastSeg = -1;

  String _fmtTime(double s) {
    if (s.isNaN || s < 0) return '–:––';
    final m = s ~/ 60;
    final sec = (s % 60).floor();
    return '$m:${sec.toString().padLeft(2, '0')}';
  }

  /// P13 显示播放头：引擎真值 + 速率外推。
  /// 60Hz tick 采样 5.33ms 块步进的 playhead → 高放大（3.75s 窗）直接采样
  /// 每帧 2-4px 跳变（阶梯滚动）；paint 时按当前速率外推到此刻，滚动连续。
  /// 停播/欠载（采样未推进）→ 速率冻结 0，显示钉在最后采样点；
  /// seek 后 playhead 跳变 → 下个 tick 重锚，显示直接跟跳（不跨 seek 插值）。
  /// 无采样（纯 widget 测试直设 playhead.value）回退真值。
  double get displayPlayhead {
    final at = _phSampleAt;
    if (at == null) return playhead.value;
    final dt = DateTime.now().difference(at).inMicroseconds / 1e6;
    return _phSample + _phExtrapRate * dt;
  }

  /// P13 外推采样更新（updateFromWire 每 tick 调用；独立出来便于单测，
  /// 不经桥）：playhead 推进 → 重锚并按有效速率外推（同步中 rate =
  /// bpm/gridBpm 才与引擎实际推进一致）；连续不变 = 停播/欠载 → 冻结，
  /// 显示不越过引擎真值。
  void updatePhSample(double ph, double playing, double bpm, double gridBpm) {
    if (_phSampleAt == null || ph != _phSample) {
      _phSample = ph;
      _phSampleAt = DateTime.now();
      _phExtrapRate = playing != 0 ? (gridBpm > 0 ? bpm / gridBpm : 1.0) : 0.0;
    } else {
      _phExtrapRate = 0;
    }
  }

  /// 60Hz tick：分发快照到各 notifier。
  void updateFromWire(DeckSnapshotWire s) {
    duration.value = s.duration;
    loaded.value = s.loaded != 0;
    playing.value = s.playing != 0;
    playhead.value = s.playhead;
    updatePhSample(s.playhead, s.playing, s.bpm, s.gridBpm);
    vu.value = s.vu;
    rate.value = s.rate;
    volume.value = s.volume;
    syncOn.value = s.sync_ != 0;
    keylockOn.value = s.keylock != 0;
    eqLow.value = s.eqLow;
    eqMid.value = s.eqMid;
    eqHigh.value = s.eqHigh;
    loopActive.value = s.loopActive != 0;
    loopIn.value = s.loopIn;
    loopOut.value = s.loopOut;

    timeText.value = '${_fmtTime(s.playhead)} / ${_fmtTime(s.duration)}';
    // 静态 BPM（loop pad 拍数匹配用）保持 grid 优先语义；
    // 显示 BPM 用变速后的有效值 effBpm（P11.2）。
    gridBpm.value = s.gridBpm;
    final staticBpm = s.gridBpm > 0 ? s.gridBpm : trackBpm;
    bpm.value = staticBpm;
    effBpm.value = s.bpm > 0 ? s.bpm : staticBpm;
    bpmKeyText.value =
        effBpm.value > 0 ? '${effBpm.value.toStringAsFixed(1)} $keyCamelot' : keyCamelot;
    // 有效速率：同步/推子锁定期间引擎速率 ≠ 滑杆位置（P10.1）
    final r = s.gridBpm > 0 ? (s.bpm / s.gridBpm - 1.0) * 100.0 : s.rate;
    effRate.value = r;
    tempoText.value = '范围 ±8%\n当前 ${r >= 0 ? '+' : ''}${r.toStringAsFixed(2)}%';

    // 播放头所在分析段（变化时告知分析线程排序）
    final seg = (s.playhead / kSegSecs).floor();
    if (seg != _lastSeg) {
      _lastSeg = seg;
      setAnalysisPriority(deck: deck, priority: seg);
    }
    waveTick.value++;
  }

  /// 载曲重置 cue 状态：cue 回到曲首 0，hotcue 全清。
  /// （loop 三总线由引擎 load 复位，快照下个 tick 反映。）
  void resetCueState() {
    cuePoint.value = 0.0;
    for (final h in hotcues) {
      h.value = null;
    }
  }

  /// 订阅一次载曲的分析事件流。
  void attachAnalysis(Stream<AnalysisEventWire> stream) {
    wave.reset();
    analysisError = null;
    stream.listen(_onEvent);
  }

  void _onEvent(AnalysisEventWire ev) {
    if (ev is AnalysisEventWire_Segment) {
      if (ev.seg + 1 > wave.segCount) wave.segCount = ev.seg + 1;
      wave.segs[ev.seg] = packCols(ev.detail);
      waveRev.value++;
    } else if (ev is AnalysisEventWire_TrackAnalysis) {
      trackBpm = ev.bpm;
      keyCamelot = ev.keyCamelot;
      beats = ev.beatsSecs;
      downbeats = ev.downbeatsSecs;
      waveRev.value++;
    } else if (ev is AnalysisEventWire_Done) {
      wave.full = packCols(ev.detail);
      wave.fullOverview = packCols(ev.overview);
      wave.framesPerCol = ev.framesPerCol;
      wave.sampleRate = ev.sampleRate;
      wave.durationFrames = ev.durationFrames.toInt();
      wave.segs.clear();
      waveRev.value++;
    } else if (ev is AnalysisEventWire_Failed) {
      analysisError = ev.msg;
      waveRev.value++;
    }
  }

  @override
  void dispose() {
    timeText.dispose();
    bpmKeyText.dispose();
    tempoText.dispose();
    playing.dispose();
    syncOn.dispose();
    keylockOn.dispose();
    rate.dispose();
    effRate.dispose();
    volume.dispose();
    vu.dispose();
    eqLow.dispose();
    eqMid.dispose();
    eqHigh.dispose();
    loaded.dispose();
    playhead.dispose();
    duration.dispose();
    waveTick.dispose();
    waveRev.dispose();
    metaRev.dispose();
    cuePoint.dispose();
    for (final h in hotcues) {
      h.dispose();
    }
    loopActive.dispose();
    loopIn.dispose();
    loopOut.dispose();
    bpm.dispose();
    gridBpm.dispose();
    effBpm.dispose();
    super.dispose();
  }
}
