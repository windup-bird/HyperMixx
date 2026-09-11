#!/usr/bin/env bash
# 双 Deck 相位差探针：两个 Deck 从 frame 0 同点起播，每隔 BEATS 拍给 deck1 一次 beatjump +BEATS，
# 记录每次跳转后的相位差。
#
#   ./scripts/phase_probe.sh                       # test.mp3 @ 122 BPM，跳 6 次
#   ./scripts/phase_probe.sh <bin> <file> <bpm> <beats> <rounds>
#
# 期望：delta 每轮精确增加 step 帧（err 列稳定在 ±几百帧以内）。
# err 逐轮变大 = 两 Deck 时钟没锁在一起；err 一次性跳变 = 网格/BPM 错。
# 跑的时候可以听：每跳一次出现一次鼓点重影，之后重影稳定存在但不会变成乱拍。
set -uo pipefail

BIN=${1:-target/release/hypermixx-cli}
FILE=${2:-test.mp3}
BPM=${3:-122}
BEATS=${4:-4}
ROUNDS=${5:-6}
# Analysis runs asynchronously after load; the grid must be ready before beatjumping.
WAIT_SECS=${6:-30}

[[ -x "$BIN" ]] || { echo "找不到可执行文件 $BIN，先 cargo build --release" >&2; exit 2; }
[[ -f "$FILE" ]] || { echo "找不到音频文件 $FILE" >&2; exit 2; }
# 本脚本只测正向步进：负拍会在曲首夹在拍 0 上（正确行为），且反向节奏需要预置起点。
# 抵消性由 cargo test -p hypermixx-audio --test beatlock 的 undoing_a_beatjump_* 覆盖。
(( BEATS > 0 )) || { echo "beats 必须是正整数（本脚本只测正向步进）" >&2; exit 2; }

INTERVAL=$(awk -v b="$BEATS" -v bpm="$BPM" 'BEGIN { printf "%.3f", b * 60 / bpm }')

# 命令逐条喂给 CLI，CLI 每条都会等引擎回答后再读下一行，所以 load 天然串行。
{
  printf 'load 0 %s %s\n' "$FILE" "$BPM"
  printf 'load 1 %s %s\n' "$FILE" "$BPM"
  printf 'play 0\nplay 1\n'
  sleep 0.3
  printf 'state\n'
  # Wait for the async analysis to publish its grid (bpm > 0 in the state output).
  echo "waiting ${WAIT_SECS}s for analysis..."
  sleep "$WAIT_SECS"
  printf 'state\n'
  for _ in $(seq 1 "$ROUNDS"); do
    sleep "$INTERVAL"
    printf 'beatjump 1 %s\n' "$BEATS"
    sleep 0.25            # 等预热线程 + 下一个块切换完成
    printf 'state\n'
  done
  printf 'quit\n'
} | "$BIN" 2>&1 | awk -v beats="$BEATS" -v bpm="$BPM" '
  # state 每次输出 deck0、deck1 两行，成对到达：[current/total] 里取 current
  BEGIN {
    step = int(beats * 48000 * 60.0 / bpm + 0.5)
    printf "\n%5s %12s %12s %10s %10s %8s %8s %7s %6s\n", \
           "round", "deck0", "deck1", "delta", "expected", "inc", "err", "beats", "ms"
  }
  match($0, /deck[01][^[]*\[[0-9]+/) {
    head = substr($0, RSTART, RLENGTH)
    frame = substr(head, index(head, "[") + 1) + 0
    if ($0 ~ /deck0/) { f0 = frame; next }
    delta = frame - f0
    # 第一对是起跳前的基线：两条 play 各等一次回答，deck1 会晚一块起步，扣掉它才看得清跳转精度
    if (!seen++) {
      base = delta
      prev = delta
      printf "%5s %12d %12d %10d %29s 基线（两 deck 起跑差）\n", "-", f0, frame, delta, ""
      next
    }
    round++
    # inc = 本轮相对上轮的相位差增量，这才是"每次跳 N 拍"的直接判据
    inc = delta - prev
    prev = delta
    err = delta - (base + round * step)
    printf "%5d %12d %12d %10d %10d %8d %+8d %7.3f %6.1f  %s\n", round, f0, frame, delta, \
           base + round * step, inc, err, (delta - base) * bpm / (48000.0 * 60.0), err / 48.0, \
           (inc > step + step / 2 || step - inc > step / 2 ? "FAIL" : "ok")
  }
  END {
    if (!round) { print "没解析到成对的 state 行：检查 CLI 是否正常启动、素材能否加载" ; exit 3 }
    printf "\n共 %d 轮，step = %d 帧 / %d 拍；基线 %+d 帧；最后一轮误差 %+d 帧（1 帧 ≈ 0.02ms）\n", \
           round, step, beats, base, err
  }
'
