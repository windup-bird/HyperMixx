// P13 拍轴窗口纯函数测试：waveWindowFor 的秒轴回退、同步跨轨对齐、
// 滚动速度与 rate 无关、播放线固定居中（首尾不钳制，删 P11.4a 尾端停右缘）。

import 'package:flutter_test/flutter_test.dart';

import 'package:hypermixx/painters/scrolling_wave_painter.dart';

void main() {
  test('无网格回退秒轴：播放线固定居中（P13，首尾不钳制）', () {
    final (ws, wt) = waveWindowFor(100, 60, 0, 0);
    expect(wt, 60, reason: '无网格窗口宽 = winSec');
    expect(ws, 70, reason: '中段播放头居中');

    final (ws2, _) = waveWindowFor(320, 60, 0, 0);
    expect(ws2, 290, reason: '接近曲尾仍居中（删尾端钳制，旧值 269）');

    final (ws3, _) = waveWindowFor(0, 60, 0, 0);
    expect(ws3, -30, reason: '曲头前留白（winStart 可为负）');
  });

  test('曲尾播到底：窗口越出曲长、指针不动（P13 固定居中）', () {
    // ph=dur=329 → winStart = 299（无钳制）→ 波形止于中线、右半留白
    final (ws, wt) = waveWindowFor(329, 60, 0, 0);
    expect(ws, 299, reason: '曲尾播放线仍居中（旧钳制值 269）');
    expect((329 - ws) / wt, 0.5, reason: '指针 x 比例 = 50% 屏宽（恒居中）');
  });

  test('拍轴：同步两轨同一乐拍落同一 x', () {
    // A：gridBpm 120，rate×1.032 → eff 123.84；ph_A=100s（乐拍 200）
    const gA = 120.0, eA = 123.84, phA = 100.0;
    // B：gridBpm 124，同步后 eff 同为 123.84；同乐拍 200 → ph_B
    const gB = 124.0, eB = 123.84;
    const phB = 200 * 60 / 124; // 96.77419…s
    const winSec = 60.0, w = 800.0;

    double xAt(double g, double e, double ph, double beat) {
      final (ws, wt) = waveWindowFor(ph, winSec, g, e);
      return (beat * 60 / g - ws) / wt * w;
    }

    for (var k = 190; k <= 210; k++) {
      final xa = xAt(gA, eA, phA, k.toDouble());
      final xb = xAt(gB, eB, phB, k.toDouble());
      expect(xa, closeTo(xb, 1e-9),
          reason: '乐拍 $k 两轨应同 x（xa=$xa xb=$xb）');
    }
    // 播头（同乐拍）也应同 x
    expect(xAt(gA, eA, phA, 200.0), closeTo(400.0, 1e-9),
        reason: '两轨播放头都在窗口中心');
  });

  test('拍轴：滚动速度 = w/winSec，与 rate 无关', () {
    const gA = 120.0, eA = 123.84, phA = 100.0;
    const gB = 124.0, eB = 123.84;
    const phB = 200 * 60 / 124;
    const winSec = 60.0, w = 800.0;
    // 同一墙钟秒：各推进 e/g 音轨秒（eff 相同 → 乐拍推进相同）
    final (wsa0, wta) = waveWindowFor(phA, winSec, gA, eA);
    final (wsa1, _) = waveWindowFor(phA + eA / gA, winSec, gA, eA);
    final (wsb0, wtb) = waveWindowFor(phB, winSec, gB, eB);
    final (wsb1, _) = waveWindowFor(phB + eB / gB, winSec, gB, eB);
    final dxa = (wsa1 - wsa0) / wta * w;
    final dxb = (wsb1 - wsb0) / wtb * w;
    expect(dxa, closeTo(w / winSec, 1e-9),
        reason: 'A 轨 1 墙钟秒窗口位移 = w/winSec px');
    expect(dxb, closeTo(w / winSec, 1e-9),
        reason: 'B 轨 1 墙钟秒窗口位移 = w/winSec px（两轨同速滚动）');
  });

  test('拍轴：单轨 rate≠1 时窗口为 60/zoom×rate 音轨秒', () {
    final (_, wt) = waveWindowFor(100, 60, 120, 123.84);
    expect(wt, closeTo(60 * 123.84 / 120, 1e-12), reason: 'winSec×eff/gridBpm');
  });
}
