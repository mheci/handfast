#!/usr/bin/env bash
# Android interop smoke: build handfastd, run it headless with a scratch
# state, and drive it through the independent Android-shaped peer
# (tools/interop/android_peer.py). Exits non-zero on any failure.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRATCH="$(mktemp -d)"
DAEMON_PID=""
cleanup() {
    if [ -n "$DAEMON_PID" ]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$SCRATCH"
}
trap cleanup EXIT

echo "==> building handfastd"
cargo build -p handfastd --manifest-path "$REPO_ROOT/Cargo.toml"

echo "==> starting handfastd (headless, scratch state)"
export HOME="$SCRATCH/home"
export XDG_CONFIG_HOME="$SCRATCH/config"
export XDG_DATA_HOME="$SCRATCH/data"
export XDG_CACHE_HOME="$SCRATCH/cache"
export XDG_RUNTIME_DIR="$SCRATCH/runtime"
mkdir -p "$HOME/Downloads" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

"$REPO_ROOT/target/debug/handfastd" --name "Handfast Interop Target" \
    >"$SCRATCH/daemon.log" 2>&1 &
DAEMON_PID=$!

# Wait for the TLS control port (KDE Connect well-known 1716) to accept.
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

echo "==> running android-shaped interop peer"
HANDFAST_CERT_DIR="$XDG_CONFIG_HOME/handfast" \
python3 "$REPO_ROOT/tools/interop/android_peer.py"

echo "==> interop smoke passed"
