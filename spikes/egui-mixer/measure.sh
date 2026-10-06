#!/usr/bin/env bash
# Spike (GitHub #21): measure memory and CPU of the egui mixer on Linux.
#
# Usage: ./measure.sh [--auto]
#   Builds the release binary, starts it, and prints:
#     1. RSS and CPU 10 s after start (meters running)
#     2. RSS and CPU after 5 minutes (meters running)
#     3. CPU with the meters paused
#   Step 3 needs the "メーター停止" toggle to be pressed. Without --auto the
#   script waits for Enter; with --auto it clicks the toggle with xdotool.
#
# CPU % is the average over a 30 s window from /proc/<pid>/stat
# (utime + stime), where 100 % = one full core.
set -euo pipefail

cd "$(dirname "$0")"
BIN=target/release/egui-mixer
WINDOW_SECS=30
TICKS=$(getconf CLK_TCK)

cargo build --release --quiet

rss_mb() {
  awk '/^VmRSS:/ { printf "%.1f", $2 / 1024 }' "/proc/$1/status"
}

cpu_ticks() {
  # Fields 14 and 15 of /proc/<pid>/stat are utime and stime.
  awk '{ print $14 + $15 }' "/proc/$1/stat"
}

cpu_percent() {
  local pid=$1 before after
  before=$(cpu_ticks "$pid")
  sleep "$WINDOW_SECS"
  after=$(cpu_ticks "$pid")
  awk -v d=$((after - before)) -v t="$TICKS" -v s="$WINDOW_SECS" \
    'BEGIN { printf "%.1f", d / t / s * 100 }'
}

report() {
  local label=$1 pid=$2 cpu
  cpu=$(cpu_percent "$pid")
  printf '%-28s RSS %7s MiB   CPU %5s %%\n' "$label" "$(rss_mb "$pid")" "$cpu"
}

"$BIN" 2>/dev/null &
PID=$!
trap 'kill $PID 2>/dev/null || true' EXIT

echo "machine: $(uname -srm), $(nproc) cores, total memory $(free -h | awk '/^Mem:/ { print $2 }')"
sleep 10
report "1. after 10 s (meters on)" "$PID"

# Wait until 5 minutes after start (10 s + 30 s already elapsed).
sleep $((300 - 10 - WINDOW_SECS))
report "2. after 5 min (meters on)" "$PID"

if [[ "${1:-}" == "--auto" ]]; then
  WID=$(xdotool search --pid "$PID" --name "egui spike" | head -1)
  # The toggle sits in the toolbar; click it relative to the window.
  xdotool mousemove --window "$WID" 225 13 click 1
  # Move the pointer off the window so hover does not trigger repaints.
  xdotool mousemove 0 0
  sleep 2
else
  read -r -p "Press 'メーター停止' in the window, move the mouse away, then press Enter: "
fi
report "3. meters paused" "$PID"
