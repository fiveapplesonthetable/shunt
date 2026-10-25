#!/usr/bin/env bash
# Runs the whole workspace test suite under one overall time cap.
#
#   scripts/test-suite.sh [extra cargo test args...]
#
# Environment:
#   TACHO_SUITE_CAP_SECS  overall cap in seconds (default 3000 = 50 min)
#   TACHO_SUITE_LOG       log file (default target/test-suite.log)
#
# Output is shown live and copied to the log. If the cap expires, the script
# prints a hang report (the test binary that was running, its live test
# processes, any test libtest flagged as long-running, and the last 40 lines
# of output), kills the run and exits 124. Otherwise it exits with cargo's
# status.
#
# The cap is enforced by a blocking `read -t` on cargo's output: the script
# sleeps in the kernel until a line arrives or the cap expires; nothing polls.
set -euo pipefail

cap=${TACHO_SUITE_CAP_SECS:-3000}
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
log=${TACHO_SUITE_LOG:-$root/target/test-suite.log}
tail_lines=40

mkdir -p "$(dirname "$log")"
: >"$log"

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
fifo=$workdir/out
mkfifo "$fifo"

# Job control gives cargo its own process group (pgid == pid), so the whole
# tree, test binaries included, can be listed and killed together.
set -m
(cd "$root" && exec cargo test --workspace --no-fail-fast "$@") >"$fifo" 2>&1 &
cargo_pid=$!
set +m
exec {out}<"$fifo"

current_binary="(none yet)"
long_running=()
deadline=$((SECONDS + cap))
timed_out=0

while :; do
  left=$((deadline - SECONDS))
  if ((left <= 0)); then
    timed_out=1
    break
  fi
  status=0
  IFS= read -r -t "$left" -u "$out" line || status=$?
  if ((status > 128)); then
    timed_out=1
    break
  fi
  # On EOF a final unterminated line may still be in `line`.
  if ((status == 0)) || [[ -n $line ]]; then
    printf '%s\n' "$line" | tee -a "$log"
    case $line in
      *"Running "*) current_binary=${line#*Running } ;;
      *"has been running for over"*) long_running+=("$line") ;;
    esac
  fi
  ((status == 0)) || break
done

if ((timed_out)); then
  {
    echo
    echo "hang: test suite exceeded its ${cap}s cap (TACHO_SUITE_CAP_SECS)"
    echo "running test binary: $current_binary"
    echo "live test processes:"
    pgrep -a -g "$cargo_pid" || echo "  (none)"
    if ((${#long_running[@]})); then
      echo "tests flagged as long-running by libtest:"
      printf '  %s\n' "${long_running[@]}"
    fi
    echo "--- last $tail_lines lines of output ($log) ---"
    tail -n "$tail_lines" "$log"
  } >&2
  kill -KILL -- "-$cargo_pid" || echo "process group $cargo_pid already gone" >&2
  wait "$cargo_pid" || true
  exit 124
fi

wait "$cargo_pid"
