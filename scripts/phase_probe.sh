#!/usr/bin/env bash
# 双 Deck 相位差探针：两个 Deck 从 frame 0 同点起播，每隔 BEATS 拍给 deck1 一次 beatjump +BEATS，
# 记录每次跳转后的相位差。
#
#   ./scripts/phase_probe.sh                                    # test.mp3，122 BPM 估计，跳 6 次
#   ./scripts/phase_probe.sh <bin> <file> <bpm> <beats> <rounds> [wait]
#
# 判据（按优先级）：
#   1. inc 恒定  → beatjump 每次跳的距离一致 = 精确（这是核心判据）
#   2. err ≈ 0   → 网格 BPM 恰好匹配脚本参数；若分析出的 BPM 不同，err 会偏但不影响精度
#
# 跑的时候可以听：每跳一次出现一次鼓点重影，重影间距恒定 = 相位锁定。
set -uo pipefail

BIN=${1:-target/release/hypermixx-cli}
FILE=${2:-test.mp3}
BPM=${3:-122}
BEATS=${4:-16}
ROUNDS=${5:-6}
# Analysis runs asynchronously after load; the grid must be ready before beatjumping.
WAIT_SECS=${6:-30}

[[ -x "$BIN" ]] || { echo "找不到可执行文件 $BIN，先 cargo build --release" >&2; exit 2; }
[[ -f "$FILE" ]] || { echo "找不到音频文件 $FILE" >&2; exit 2; }
# 本脚本只测正向步进：负拍会在曲首夹在拍 0 上（正确行为），且反向节奏需要预置起点。
# 抵消性由 cargo test -p hypermixx-audio --test beatlock 的 undoing_a_beatjump_* 覆盖。
(( BEATS > 0 )) || { echo "beats 必须是正整数（本脚本只测正向步进）" >&2; exit 2; }

INTERVAL=$(awk -v b="$BEATS" -v bpm="$BPM" 'BEGIN { printf "%.3f", b * 60 / bpm }')

# 命令逐条喂给 CLI。load 现在在 CLI 侧后台解码后才把 source 送进引擎，所以要等 Loaded
# 落地再 play/jump，否则命令会打到空 deck 上被拒。给每次 load 留出解码时间。
{
  printf 'load 0 %s %s\n' "$FILE" "$BPM"
  printf 'load 1 %s %s\n' "$FILE" "$BPM"
  sleep "$WAIT_SECS"          # 等两路解码完成、deck 装好并装填常网格
  printf 'play 0\nplay 1\n'
  sleep 0.3
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
    nominal_step = int(beats * 44100 * 60.0 / bpm + 0.5)
    printf "\n%5s %12s %12s %10s %10s %10s %8s %7s  %s\n", \
           "round", "deck0", "deck1", "delta", "expected", "err", "ms", "inc", "判定"
  }
  match($0, /deck[01][^[]*\[[0-9]+/) {
    head = substr($0, RSTART, RLENGTH)
    frame = substr(head, index(head, "[") + 1) + 0
    if ($0 ~ /deck0/) { f0 = frame; next }
    delta = frame - f0
    # 第一对是起跳前的基线（两 deck 起跑差）
    if (!seen++) {
      base = delta
      prev = delta
      printf "%5s %12d %12d %10d %49s 基线（两 deck 起跑差）\n", "-", f0, frame, delta, ""
      next
    }
    round++
    inc = delta - prev
    prev = delta
    # 首轮实测步长作为基准：之后每轮理论相位差 = base + round*step_real。
    # 不用脚本参数 BPM，因为分析网格的实际 BPM 可能不同；用实测才公平。
    if (round == 1) step_real = inc
    expected = base + round * step_real
    err = delta - expected
    ms = err / 48.0
    # err 恒定（|err| 在块量化 + 浮点舍入范围内）= 每跳距离一致 = beatjump 精确。
    verdict = (err < 512 && err > -512) ? "ok" : "FAIL"
    printf "%5d %12d %12d %10d %10d %+10d %8.1f %7d  %s\n", \
           round, f0, frame, delta, expected, err, ms, inc, verdict
  }
  END {
    if (!round) { print "没解析到成对的 state 行：检查 CLI 是否正常启动、素材能否加载" ; exit 3 }
    printf "\n共 %d 轮；实测步长 step_real = %d 帧（脚本假设 %d BPM → %d 帧）\n", \
           round, step_real, bpm, nominal_step
    print "err 在 ±512 帧内 = beatjump 每次跳等距（精确）；step_real 与 nominal_step 的差只是网格 BPM 与参数不同，不代表不准。"
  }
'
