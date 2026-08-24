#!/usr/bin/env bash
# perf.sh — idle resource benchmark for the handfastd daemon.
#
# What it does:
#   1. Builds target/release/handfastd if it is not present.
#   2. Starts handfastd in the background with an isolated temp state dir,
#      so a real user profile or running instance is never touched.
#   3. Samples RSS from /proc/PID/status (VmRSS) every 500 ms for 30 s.
#   4. Reports peak RSS, average RSS and average CPU% (from the utime+stime
#      delta in /proc/PID/stat).
#   5. Kills handfastd (also on Ctrl-C / errors via trap).
#   6. Validates against release targets:
#        peak RSS < 40 MB
#        average CPU < 1 %
#
# Exit status: 0 = all targets met, 1 = target missed or setup error.
#
# Requires: Linux (/proc), bash, awk, getconf. Run from the repo root.
#
# Keep this file executable:  chmod +x scripts/perf.sh
set -euo pipefail

readonly BIN="target/release/handfastd"
readonly DURATION_S=30
readonly INTERVAL_S=0.5
readonly LIMIT_PEAK_KB=$((40 * 1024)) # 40 MB (MiB) ceiling for peak RSS
readonly LIMIT_CPU_PCT=1              # 1 % ceiling for average idle CPU

DAEMON_PID=""
TMPDIR_PERF=""

log() { printf '[perf] %s\n' "$*"; }
die() { printf '[perf] ERROR: %s\n' "$*" >&2; exit 1; }

[[ "$(uname -s)" == "Linux" ]] || die "this benchmark reads /proc and must run on Linux"
command -v getconf >/dev/null 2>&1 || die "getconf not found"
command -v awk >/dev/null 2>&1 || die "awk not found"

cleanup() {
    if [[ -n "$DAEMON_PID" ]]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    [[ -n "$TMPDIR_PERF" ]] && rm -rf "$TMPDIR_PERF"
}
trap cleanup EXIT

# ---------------------------------------------------------------- build ----
if [[ ! -x "$BIN" ]]; then
    log "release binary missing; building handfastd (cargo build --release)"
    cargo build --release -p handfastd
fi
[[ -x "$BIN" ]] || die "expected binary $BIN was not produced"

# ---------------------------------------------------------------- start ----
TMPDIR_PERF="$(mktemp -d)"
log "starting handfastd (pid pending) with isolated state in $TMPDIR_PERF"
"$BIN" --socket "$TMPDIR_PERF/handfast.sock" --data-dir "$TMPDIR_PERF/state" >/dev/null 2>&1 &
DAEMON_PID=$!
log "handfastd started, pid $DAEMON_PID"

# Give the process up to 5 s to come up before sampling.
up=0
for _ in $(seq 1 50); do
    kill -0 "$DAEMON_PID" 2>/dev/null || die "handfastd exited during startup"
    [[ -d "/proc/$DAEMON_PID" ]] && { up=1; break; }
    sleep 0.1
done
[[ "$up" -eq 1 ]] || die "/proc/$DAEMON_PID never appeared"

rss_kb() { awk '/^VmRSS:/ { print $2 }' "/proc/$DAEMON_PID/status"; }
cpu_ticks() { awk '{ print $14 + $15 }' "/proc/$DAEMON_PID/stat"; } # utime+stime

# --------------------------------------------------------------- sample ----
readonly CLK_TCK="$(getconf CLK_TCK)" # tick rate for /proc/PID/stat fields
readonly SAMPLES_TOTAL=$(awk -v d="$DURATION_S" -v i="$INTERVAL_S" \
    'BEGIN { printf "%d", d / i }')

ticks_start="$(cpu_ticks)" || die "cannot read /proc/$DAEMON_PID/stat"
start_s="$(date +%s.%N)"

samples=0
sum_kb=0
peak_kb=0
log "sampling RSS ${INTERVAL_S}s x $SAMPLES_TOTAL (${DURATION_S}s window)"
while (( samples < SAMPLES_TOTAL )); do
    kb="$(rss_kb)" || true
    if [[ -z "$kb" ]]; then
        kill -0 "$DAEMON_PID" 2>/dev/null || die "handfastd died mid-benchmark (after $samples samples)"
        die "could not read VmRSS from /proc/$DAEMON_PID/status"
    fi
    samples=$((samples + 1))
    sum_kb=$((sum_kb + kb))
    (( kb > peak_kb )) && peak_kb=$kb
    sleep "$INTERVAL_S"
done

ticks_end="$(cpu_ticks)" || die "handfastd died mid-benchmark (after $samples samples)"
end_s="$(date +%s.%N)"

# ---------------------------------------------------------------- report ---
read -r peak_mb avg_mb avg_cpu_pct elapsed_s <<<"$(awk -v pk="$peak_kb" -v sm="$sum_kb" \
    -v n="$samples" -v tk="$((ticks_end - ticks_start))" -v hz="$CLK_TCK" \
    -v t0="$start_s" -v t1="$end_s" 'BEGIN {
        el = t1 - t0; if (el <= 0) el = 1
        printf "%.2f %.2f %.2f %.2f\n", pk / 1024, sm / n / 1024, (tk / hz) / el * 100, el
    }')"

log "-------------------- results -------------------"
printf '[perf] %-28s %8.2f MB\n' "peak RSS:" "$peak_mb"
printf '[perf] %-28s %8.2f MB\n' "average RSS:" "$avg_mb"
printf '[perf] %-28s %8.2f %%\n' "average CPU (${elapsed_s}s window):" "$avg_cpu_pct"
printf '[perf] %-28s %8d samples\n' "samples taken:" "$samples"
log "------------------------------------------------"

# ------------------------------------------------------------ validate ----
fail=0
if awk -v v="$peak_kb" -v lim="$LIMIT_PEAK_KB" 'BEGIN { exit !(v < lim) }'; then
    log "PASS: peak RSS below ${LIMIT_PEAK_KB} kB ($((LIMIT_PEAK_KB / 1024)) MB)"
else
    log "FAIL: peak RSS above ${LIMIT_PEAK_KB} kB ($((LIMIT_PEAK_KB / 1024)) MB)"
    fail=1
fi
if awk -v v="$avg_cpu_pct" -v lim="$LIMIT_CPU_PCT" 'BEGIN { exit !(v < lim) }'; then
    log "PASS: average CPU below ${LIMIT_CPU_PCT} %"
else
    log "FAIL: average CPU at or above ${LIMIT_CPU_PCT} %"
    fail=1
fi

if (( fail == 0 )); then
    log "overall: PASS"
else
    log "overall: FAIL (see docs/perf.md for targets and baselines)"
fi
exit "$fail"
