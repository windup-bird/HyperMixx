#!/usr/bin/env bash
# 双 Deck 稳态相位差探针：deck0 / deck1 分别载入同一文件，**先后起播**制造一个相位差，
# 然后对 deck1 发一次 `sync <mode>`，每拍采样一次两 deck 的拍相位，打印收敛过程与
# **稳态相位差**（末尾若干轮的均值）。
#
#   ./scripts/sync_phase.sh                                          # 默认 pid，test.mp3 122 BPM
#   ./scripts/sync_phase.sh <bin> <file> <bpm> <gap> <rounds> [mode]
#
#   <gap>     deck0 先走多少拍再起 deck1（默认 0.75 拍）——用拍而不是秒：秒换个 BPM 就
#             可能恰好落成整数拍，相位差直接归零、看不出追相位到底有没有在干活
#   <rounds>  采样轮数，一轮 = 1 拍（默认 24）
#   mode      pid | linear | instant | tempo | none（默认 pid）
#             linear 的时长走环境变量 LIN_T，默认 2.0 秒
#   最后一个参数是等待解码的上限秒数（默认 60）——脚本不盲等，看到两路 `loaded:` 就继续。
#
# 判据：
#   1. 追相位（pid/linear/instant）：末 5 轮 |diff| 均值 < 0.05 拍
#   2. tempo / none（不追相位）：稳态 |diff| 与基线一致 ⇒ 同速、相位差恒定，偏差 < 0.02 拍
#   3. 任何一轮播放头没走 ⇒ 直接失败（加载/播放没起来，读数全是 0 不算通过）
#
# 跑的时候可以听：先后起播会出现一次鼓点重影，追上之后重影应当重合成一个。
set -uo pipefail

BIN=${1:-target/release/hypermixx-cli}
FILE=${2:-test.mp3}
BPM=${3:-122}
GAP=${4:-0.75}
ROUNDS=${5:-24}
MODE=${6:-pid}
WAIT_SECS=${7:-60}
LIN_T=${LIN_T:-2.0}
# 稳态统计取最后几轮。
STEADY=${STEADY:-5}

[[ -x "$BIN" ]] || { echo "找不到可执行文件 $BIN，先 cargo build --release" >&2; exit 2; }
[[ -f "$FILE" ]] || { echo "找不到音频文件 $FILE" >&2; exit 2; }

INTERVAL=$(awk -v b="$BPM" 'BEGIN { printf "%.3f", 60.0 / b }')
# 起播差按拍给，实际 sleep 换成秒。载入用固定 BPM，常网格下拍长是确定的，换算因此是稳的。
GAP_S=$(awk -v g="$GAP" -v b="$BPM" 'BEGIN { printf "%.3f", g * 60.0 / b }')

# 追相位的模式先 `sync tempo` 再装控制器，所以模式直接翻成一条 sync 命令。
SYNC_CMD=""
case "$MODE" in
  pid)     SYNC_CMD="deck1 sync phase pid" ;;
  linear)  SYNC_CMD="deck1 sync phase linear $LIN_T" ;;
  instant) SYNC_CMD="deck1 sync phase instant" ;;
  tempo)   SYNC_CMD="deck1 sync tempo" ;;
  none)    SYNC_CMD="" ;;
  *) echo "未知 mode \`$MODE\` — pid|linear|instant|tempo|none" >&2; exit 2 ;;
esac

OUT=$(mktemp /tmp/sync_phase.XXXXXX)
IN=$(mktemp -u /tmp/sync_phase_in.XXXXXX)
mkfifo "$IN"
cleanup() { exec 3>&- 2>/dev/null; [[ -n "${CLI_PID:-}" ]] && kill "$CLI_PID" 2>/dev/null; rm -f "$IN" "$OUT"; }
trap cleanup EXIT

# stdin 走 FIFO、stdout 落文件：这样本脚本可以边发命令边看 CLI 什么时候真的装载完，
# 而不是对着解码速度赌一个固定 sleep。
"$BIN" <"$IN" >"$OUT" 2>&1 &
CLI_PID=$!
exec 3>"$IN"
send() { printf '%s\n' "$1" >&3; }

send "deck0 load $FILE $BPM"
send "deck1 load $FILE $BPM"

# 等两路都报 loaded，最多 WAIT_SECS 秒。
ready=0
for _ in $(seq 1 $((WAIT_SECS * 2))); do
  if [[ $(grep -c 'loaded:' "$OUT" 2>/dev/null || true) -ge 2 ]]; then ready=1; break; fi
  sleep 0.5
done
if [[ $ready -ne 1 ]]; then
  echo "两路解码在 ${WAIT_SECS}s 内没完成；CLI 输出：" >&2
  sed 's/^/  /' "$OUT" >&2
  exit 2
fi

# 先后播放：deck0 先走 GAP 拍，再起 deck1 —— 两者之间就有了一个真实的相位差。
send "deck0 play"
sleep "$GAP_S"
send "deck1 play"
sleep 0.5
send "state"                       # 基线
if [[ -n "$SYNC_CMD" ]]; then
  send "$SYNC_CMD"
  sleep 0.3                        # 命令落块
fi
for _ in $(seq 1 "$ROUNDS"); do
  sleep "$INTERVAL"
  send "state"
done
send "quit"
exec 3>&-
# Bounded wait: a CLI that never gets `quit` (or never flushes it) must not wedge the script —
# the trap kills it either way, but only after we have given it a fair chance to exit cleanly.
for _ in $(seq 1 25); do
  kill -0 "$CLI_PID" 2>/dev/null || break
  sleep 0.2
done
kill "$CLI_PID" 2>/dev/null
wait "$CLI_PID" 2>/dev/null
CLI_PID=""

awk -v bpm="$BPM" -v mode="$MODE" -v steady="$STEADY" -v gap="$GAP_S" -v gap_beats="$GAP" '
  # NB: this program lives inside one shell single-quote — it must not contain an ASCII
  # apostrophe (line/dont/its ...) or the quote ends there and bash starts parsing awk as shell.
  function wrap(x) { if (x > 0.5) x -= 1.0; else if (x < -0.5) x += 1.0; return x }
  # 常网格下相位 = 拍内位置；网格存的是 round(i * fpb)，与理想 fpb 的偏差不到半帧。
  function phase(f,   r) { r = f / fpb; return r - int(r) }
  function beats(ms) { return ms * bpm / 60000.0 }

  BEGIN {
    fpb = 44100 * 60.0 / bpm
    printf "\n%5s %12s %12s %9s %9s %10s %8s\n", \
           "round", "deck0", "deck1", "phase0", "phase1", "diff", "ms"
  }

  # 引擎的拒绝要露出来，否则"全 0"看起来会像一次成功的收敛。
  /^[Ee]rror/ { printf "  CLI: %s\n", $0; errors++ }

  # state 每次输出 deck0、deck1 两行，成对到达：位置取 `[` 里的 current（徽标跟在后面，不进匹配）。
  match($0, /deck[01][^[]*\[[0-9]+/) {
    head = substr($0, RSTART, RLENGTH)
    frame = substr(head, index(head, "[") + 1) + 0
    # Identify the deck from the line-own label (the match starts at column 1), never by
    # searching the whole line: a sync badge appends "<- deck0" to the deck1 line, which would
    # otherwise make every post-sync reading look like a deck0 line and never pair up.
    if (substr(head, 5, 1) == "0") { f0 = frame; next }
    f1 = frame
    if (f0 > maxf) maxf = f0
    if (f1 > maxf) maxf = f1
    p0 = phase(f0); p1 = phase(f1)
    d = wrap(p0 - p1)
    n++
    if (n == 1) { base = d; tag = "基线（起播差）" } else { tag = "" }
    diffs[n] = d
    ms = d * 60000.0 / bpm
    printf "%5d %12d %12d %9.4f %9.4f %+10.4f %8.1f  %s\n", \
           (n == 1 ? 0 : n - 1), f0, f1, p0, p1, d, ms, tag
  }

  END {
    if (maxf == 0) {
      print "\n播放头一帧没走：素材没装载或没起播，读数全 0 不算通过。"
      exit 3
    }
    if (n < 2) {
      print "没解析到成对的 state 行：检查 CLI 是否正常启动、素材能否加载"
      exit 3
    }
    # 稳态 = 末 steady 轮采样，取绝对值均值。
    start = n - steady + 1
    if (start < 2) start = 2
    sum = 0; cnt = 0
    for (i = start; i <= n; i++) { sum += (diffs[i] < 0 ? -diffs[i] : diffs[i]); cnt++ }
    steady_diff = (cnt ? sum / cnt : 0)
    drift = diffs[n] - base
    base_abs = (base < 0 ? -base : base)

    printf "\n基线相位差   %+0.4f 拍 (%+0.1f ms)，起播差 %s 秒 = %s 拍\n", \
           base, base * 60000.0 / bpm, gap, gap_beats
    printf "稳态相位差   %.4f 拍 (%.1f ms)  —— 末 %d 轮 |diff| 均值\n", \
           steady_diff, steady_diff * 60000.0 / bpm, cnt
    printf "总漂移       %+0.4f 拍（末轮 − 基线）\n", drift

    if (mode == "pid" || mode == "linear" || mode == "instant") {
      verdict = (steady_diff < 0.05) ? "ok" : "FAIL"
      reason = (verdict == "ok") ? \
        "相位差收敛到 0.05 拍以内" : "稳态相位差过大 = 没追上"
    } else {
      # 不追相位：同素材同网格 ⇒ 同速 ⇒ 相位差应当原样停在起播差上。
      offset = steady_diff - base_abs
      if (offset < 0) offset = -offset
      verdict = (offset < 0.02) ? "ok" : "FAIL"
      reason = (verdict == "ok") ? \
        "相位差恒定 = 两 deck 同速（相位要靠 phase 修）" : "相位差在漂移 = 两 deck 不同速"
    }
    if (verdict == "ok" && errors > 0) {
      verdict = "FAIL"; reason = "引擎拒绝过命令（见上面的 CLI: 行）"
    }
    printf "\nmode=%-8s 判定 %s —— %s\n", mode, verdict, reason
    if (verdict == "FAIL") exit 1
  }
' "$OUT"
