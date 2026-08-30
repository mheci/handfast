#!/usr/bin/env bash
# Dial-out interop smoke: prove handfast's outbound connect reliability end to
# end against the Android-shaped peer (tools/interop/android_peer.py --serve).
#
# Handfast is the TCP *dialer* / TLS *server* here (the mirror image of
# run.sh): the peer accepts, reads our plaintext identity, upgrades as TLS
# client, re-checks the secure identity, requests pairing, and receives a file
# we push via the real `hfctl send` IPC path. The pairing request is answered
# through the interactive `hfctl pair-answer` CLI. Exits non-zero on failure.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRATCH="$(mktemp -d)"
DAEMON_PID=""
PEER_PID=""
PEER_ID="interoptest0000000000000000000000"
PEER_PORT=1717
cleanup() {
    if [ -n "$PEER_PID" ]; then kill "$PEER_PID" 2>/dev/null || true; fi
    if [ -n "$DAEMON_PID" ]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$SCRATCH"
}
trap cleanup EXIT

# Fail fast if a stale daemon already owns the control port; a leftover
# handfastd would silently absorb the readiness probe below.
if (exec 3<>/dev/tcp/127.0.0.1/1716) 2>/dev/null; then
    exec 3>&- 3<&-
    echo "port 1716 is already in use — kill the stale handfastd first" >&2
    exit 1
fi

echo "==> building handfastd + hfctl"
cargo build -p handfastd -p handfast-tui --manifest-path "$REPO_ROOT/Cargo.toml"

echo "==> starting handfastd (headless, scratch state)"
export HOME="$SCRATCH/home"
export XDG_CONFIG_HOME="$SCRATCH/config"
export XDG_DATA_HOME="$SCRATCH/data"
export XDG_CACHE_HOME="$SCRATCH/cache"
export XDG_RUNTIME_DIR="$SCRATCH/runtime"
mkdir -p "$HOME/Downloads" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

"$REPO_ROOT/target/debug/handfastd" --name "Handfast Dial-Out Target" \
    >"$SCRATCH/daemon.log" 2>&1 &
DAEMON_PID=$!

ready=0
for _ in $(seq 1 100); do
    if (exec 3<>/dev/tcp/127.0.0.1/1716) 2>/dev/null; then
        exec 3>&- 3<&-
        ready=1
        break
    fi
    sleep 0.1
done
if [ "$ready" != "1" ]; then
    echo "handfastd never opened port 1716; log tail:" >&2
    tail -20 "$SCRATCH/daemon.log" >&2 || true
    exit 1
fi

echo "==> starting android-shaped peer in serve mode (port $PEER_PORT)"
HANDFAST_CERT_DIR="$XDG_CONFIG_HOME/handfast" \
python3 "$REPO_ROOT/tools/interop/android_peer.py" --serve \
    --control-port "$PEER_PORT" --save-dir "$SCRATCH/received" \
    >"$SCRATCH/peer.log" 2>&1 &
PEER_PID=$!

echo "==> waiting for handfast to dial and request pairing..."
paired=0
for _ in $(seq 1 150); do
    if grep -q "PAIRING_REQUEST_SENT" "$SCRATCH/peer.log" 2>/dev/null; then
        paired=1
        break
    fi
    if ! kill -0 "$PEER_PID" 2>/dev/null; then
        echo "peer exited early; log tail:" >&2
        tail -30 "$SCRATCH/peer.log" >&2 || true
        exit 1
    fi
    sleep 0.2
done
if [ "$paired" != "1" ]; then
    echo "handfast never dialed the peer; daemon log tail:" >&2
    tail -30 "$SCRATCH/daemon.log" >&2 || true
    echo "peer log tail:" >&2
    tail -30 "$SCRATCH/peer.log" >&2 || true
    exit 1
fi
cat "$SCRATCH/peer.log"

echo "==> answering the pairing request (interactive prompt path)"
"$REPO_ROOT/target/debug/hfctl" pair-answer "$PEER_ID" --accept

echo "==> sending a file to the paired peer via hfctl send"
SRC="$SCRATCH/source.bin"
head -c 262144 /dev/urandom > "$SRC"
"$REPO_ROOT/target/debug/hfctl" send "$SRC" -d "$PEER_ID"

echo "==> waiting for the peer to finish receiving + reconnect check"
wait "$PEER_PID"
PEER_EXIT=$?
if [ "$PEER_EXIT" != "0" ]; then
    echo "peer failed (exit $PEER_EXIT); log tail:" >&2
    tail -40 "$SCRATCH/peer.log" >&2 || true
    exit 1
fi

RECV="$SCRATCH/received/source.bin"
if [ ! -f "$RECV" ]; then
    echo "peer did not write the received file" >&2
    exit 1
fi
if ! cmp -s "$SRC" "$RECV"; then
    echo "received payload differs from source" >&2
    exit 1
fi
echo "received payload verified: $(stat -c %s "$RECV") bytes, byte-identical"

echo "==> devices now show the peer as paired"
"$REPO_ROOT/target/debug/hfctl" devices || true

echo "==> dial-out interop passed"
