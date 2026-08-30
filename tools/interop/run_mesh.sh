#!/usr/bin/env bash
# Full-mesh E2E: two real handfastd instances discover each other over UDP,
# pair through the interactive IPC path, and exchange files in BOTH
# directions — no python peer involved.
#
# Requires the discovery socket to share UDP 1716 across daemons (SO_REUSEADDR,
# mirroring android's ShareAddress) and a distinct --tcp-port per daemon.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRATCH="$(mktemp -d)"
PID_A=""
PID_B=""
cleanup() {
    [ -n "$PID_A" ] && kill "$PID_A" 2>/dev/null || true
    [ -n "$PID_B" ] && kill "$PID_B" 2>/dev/null || true
    rm -rf "$SCRATCH"
}
trap cleanup EXIT

# Fail fast if a stale daemon owns either control port.
for PORT in 1716 1717; do
    if (exec 3<>/dev/tcp/127.0.0.1/$PORT) 2>/dev/null; then
        exec 3>&- 3<&-
        echo "port $PORT is already in use — kill the stale handfastd first" >&2
        exit 1
    fi
done

echo "==> building handfastd + hfctl"
cargo build -p handfastd -p handfast-tui --manifest-path "$REPO_ROOT/Cargo.toml"

mkdir -p "$SCRATCH/a/home/Downloads" "$SCRATCH/a/runtime" \
         "$SCRATCH/b/home/Downloads" "$SCRATCH/b/runtime"
chmod 700 "$SCRATCH/a/runtime" "$SCRATCH/b/runtime"

start_daemon() {
    local side="$1" port="$2" name="$3"
    local env_base=(
        "HOME=$SCRATCH/$side/home"
        "XDG_CONFIG_HOME=$SCRATCH/$side/config"
        "XDG_DATA_HOME=$SCRATCH/$side/data"
        "XDG_CACHE_HOME=$SCRATCH/$side/cache"
        "XDG_RUNTIME_DIR=$SCRATCH/$side/runtime"
    )
    env "${env_base[@]}" "$REPO_ROOT/target/debug/handfastd" \
        --name "$name" --tcp-port "$port" >"$SCRATCH/$side/daemon.log" 2>&1 &
    echo "$!"  # pid
}

run_hfctl() {
    local side="$1"
    shift
    env \
        "HOME=$SCRATCH/$side/home" \
        "XDG_CONFIG_HOME=$SCRATCH/$side/config" \
        "XDG_DATA_HOME=$SCRATCH/$side/data" \
        "XDG_CACHE_HOME=$SCRATCH/$side/cache" \
        "XDG_RUNTIME_DIR=$SCRATCH/$side/runtime" \
        "$REPO_ROOT/target/debug/hfctl" "$@"
}

PID_A="$(start_daemon a 1716 "Mesh A")"
PID_B="$(start_daemon b 1717 "Mesh B")"

for PORT in 1716 1717; do
    ready=0
    for _ in $(seq 1 100); do
        if (exec 3<>/dev/tcp/127.0.0.1/$PORT) 2>/dev/null; then
            exec 3>&- 3<&-
            ready=1
            break
        fi
        sleep 0.1
    done
    if [ "$ready" != "1" ]; then
        echo "daemon on port $PORT never opened; logs:" >&2
        tail -10 "$SCRATCH/a/daemon.log" "$SCRATCH/b/daemon.log" >&2 || true
        exit 1
    fi
done

# ---- discovery: each daemon must see the other -----------------------------
echo "==> waiting for mutual discovery..."
B_ID=""
A_ID=""
for _ in $(seq 1 100); do
    DEVICES_A="$(run_hfctl a devices --json 2>/dev/null || true)"
    B_ID="$(echo "$DEVICES_A" | python3 -c '
import json,sys
rows=json.load(sys.stdin)
print(next((r["device_id"] for r in rows if r.get("name")=="Mesh B"), ""))' 2>/dev/null || true)"
    DEVICES_B="$(run_hfctl b devices --json 2>/dev/null || true)"
    A_ID="$(echo "$DEVICES_B" | python3 -c '
import json,sys
rows=json.load(sys.stdin)
print(next((r["device_id"] for r in rows if r.get("name")=="Mesh A"), ""))' 2>/dev/null || true)"
    if [ -n "$B_ID" ] && [ -n "$A_ID" ]; then
        break
    fi
    sleep 0.3
done
if [ -z "$B_ID" ] || [ -z "$A_ID" ]; then
    echo "mutual discovery failed (A sees: ${B_ID:-none}, B sees: ${A_ID:-none})" >&2
    tail -15 "$SCRATCH/a/daemon.log" "$SCRATCH/b/daemon.log" >&2 || true
    exit 1
fi
echo "  B=$B_ID"
echo "  discovery OK (A found $B_ID, B found $A_ID)"

# ---- pairing: A initiates, B answers interactively -------------------------
echo "==> pairing (A initiates, B answers via pair-answer)..."
run_hfctl a pair "$B_ID" >"$SCRATCH/a/pair.out" 2>&1 &
PAIR_PID=$!

answered=0
for _ in $(seq 1 100); do
    if run_hfctl b pair-answer "$A_ID" --accept >"$SCRATCH/b/pair-answer.out" 2>&1; then
        answered=1
        break
    fi
    sleep 0.2
done
wait "$PAIR_PID" || true
cat "$SCRATCH/a/pair.out"
cat "$SCRATCH/b/pair-answer.out"
if [ "$answered" != "1" ]; then
    echo "pairing never completed" >&2
    exit 1
fi

PAIRED_A="$(run_hfctl a devices --json | python3 -c "
import json,sys
rows=json.load(sys.stdin)
print(next((str(r.get('paired')) for r in rows if r.get('device_id')=='$B_ID'), '?'))")"
PAIRED_B="$(run_hfctl b devices --json | python3 -c "
import json,sys
rows=json.load(sys.stdin)
print(next((str(r.get('paired')) for r in rows if r.get('device_id')=='$A_ID'), '?'))")"
echo "  paired flags: A->B $PAIRED_A, B->A $PAIRED_B"
[ "$PAIRED_A" = "True" ] && [ "$PAIRED_B" = "True" ]

# ---- transfer: A -> B, then B -> A -----------------------------------------
echo "==> A -> B file transfer"
SRC_A="$SCRATCH/a/source-a.bin"
head -c 131072 /dev/urandom > "$SRC_A"
run_hfctl a send "$SRC_A" -d "$B_ID" >/dev/null
for _ in $(seq 1 100); do
    [ -f "$SCRATCH/b/home/Downloads/source-a.bin" ] && break
    sleep 0.1
done
cmp -s "$SRC_A" "$SCRATCH/b/home/Downloads/source-a.bin" \
    || { echo "A->B payload mismatch" >&2; exit 1; }
echo "  A->B OK ($(stat -c %s "$SRC_A") bytes byte-identical)"

echo "==> B -> A file transfer"
SRC_B="$SCRATCH/b/source-b.bin"
head -c 65536 /dev/urandom > "$SRC_B"
run_hfctl b send "$SRC_B" -d "$A_ID" >/dev/null
for _ in $(seq 1 100); do
    [ -f "$SCRATCH/a/home/Downloads/source-b.bin" ] && break
    sleep 0.1
done
cmp -s "$SRC_B" "$SCRATCH/a/home/Downloads/source-b.bin" \
    || { echo "B->A payload mismatch" >&2; exit 1; }
echo "  B->A OK ($(stat -c %s "$SRC_B") bytes byte-identical)"

echo "==> full-mesh E2E passed"
