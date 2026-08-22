import 'dart:math' as math;

class WaveShape {
  const WaveShape({
    required this.envelope,
    required this.low,
    required this.mid,
    required this.high,
  });

  final List<double> envelope;
  final List<double> low;
  final List<double> mid;
  final List<double> high;
}

WaveShape buildWaveShape({
  required List<double> low,
  required List<double> mid,
  required List<double> high,
  required double maxHeight,
  List<double>? all,
  List<double>? normalizedHeight,
}) {
  final bands = [_smooth(low), _smooth(mid), _smooth(high)];
  final source = all == null
      ? List<double>.generate(
          low.length,
          (i) => math.max(bands[0][i], math.max(bands[1][i], bands[2][i])),
        )
      : _smooth(all);
  final envelope = normalizedHeight == null
      ? source.map((v) => math.sqrt(v / 255.0) * maxHeight).toList()
      : _heightEnvelope(normalizedHeight, maxHeight);
  for (var i = 0; i < envelope.length; i++) {
    envelope[i] = envelope[i].clamp(0.0, maxHeight);
  }
  return WaveShape(
    envelope: envelope,
    low: bands[0],
    mid: bands[1],
    high: bands[2],
  );
}

List<double> _heightEnvelope(List<double> values, double maxHeight) =>
    values.map((v) => (v / 255.0 * maxHeight).clamp(0.0, maxHeight)).toList();

List<double> _smooth(List<double> values) {
  if (values.length < 3) return [...values];
  final result = List<double>.filled(values.length, 0.0);
  result[0] = values[0];
  result[values.length - 1] = values.last;
  for (var i = 1; i < values.length - 1; i++) {
    result[i] = values[i - 1] * 0.25 + values[i] * 0.5 + values[i + 1] * 0.25;
  }
  return result;
}
