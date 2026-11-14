#!/usr/bin/env bash
# Prerequisites and expected events: scripts/README.md.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(dirname -- "$script_dir")

fail() {
    printf 'interop-share: %s\n' "$*" >&2
    exit 1
}

find_binary() {
    local name=$1
    local override=$2
    if [ -n "$override" ]; then
        [ -x "$override" ] || fail "$name override is not executable: $override"
        printf '%s\n' "$override"
        return
    fi
    local configuration suffix candidate
    for configuration in release debug; do
        for suffix in '' '.exe'; do
            candidate="$repo_root/target/$configuration/$name$suffix"
            if [ -x "$candidate" ]; then
                printf '%s\n' "$candidate"
                return
            fi
        done
    done
    fail "$name not found under $repo_root/target. Build both binaries first:
  cargo build --release -p handfast-daemon --bin handfastd --bin hfctl
Set HANDFASTD or HFCTL to use another path."
}

daemon=$(find_binary handfastd "${HANDFASTD:-}")
hfctl=$(find_binary hfctl "${HFCTL:-}")

run_dir=$(mktemp -d "${TMPDIR:-/tmp}/handfast-interop.XXXXXX")
data_dir="$run_dir/data"
incoming_dir="$run_dir/incoming"
log="$run_dir/handfastd.log"
mkdir -p "$data_dir" "$incoming_dir"

identity=$("$daemon" --print-identity --data-dir "$data_dir") ||
    fail "handfastd --print-identity failed. State dir: $data_dir"
device_id=$(printf '%s\n' "$identity" |
    sed -n 's/.*"deviceId": *"\([^"]*\)".*/\1/p' | head -n 1)
[ -n "$device_id" ] || fail "could not read body.deviceId from --print-identity output"

incoming_supported=0
if "$daemon" --help 2>&1 | grep -q -- '--incoming-dir'; then
    incoming_supported=1
fi

daemon_pid=""
cleanup() {
    if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
        kill "$daemon_pid" 2>/dev/null || true
        waited=0
        while [ "$waited" -lt 50 ]; do
            kill -0 "$daemon_pid" 2>/dev/null || break
            sleep 0.1
            waited=$((waited + 1))
        done
        kill -9 "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    daemon_pid=""
}
trap cleanup EXIT
trap 'exit 130' INT TERM

daemon_args=(--data-dir "$data_dir")
if [ "$incoming_supported" -eq 1 ]; then
    daemon_args+=(--incoming-dir "$incoming_dir")
fi
RUST_LOG=debug "$daemon" "${daemon_args[@]}" >"$log" 2>&1 &
daemon_pid=$!

started=0
waited=0
while [ "$waited" -lt 300 ]; do
    if grep -q 'discovery listening' "$log" 2>/dev/null; then
        started=1
        break
    fi
    kill -0 "$daemon_pid" 2>/dev/null || break
    sleep 0.1
    waited=$((waited + 1))
done
if [ "$started" -ne 1 ]; then
    printf 'interop-share: handfastd did not start. Log: %s\n' "$log" >&2
    tail -n 30 "$log" >&2 || true
    exit 1
fi

tcp_port=$(sed -n 's/.*TCP listener bound tcp_port=\([0-9]*\).*/\1/p' "$log" | tail -n 1)
fingerprint=$(sed -n 's/.*TLS identity loaded fingerprint=\([0-9a-f]*\).*/\1/p' "$log" | head -n 1)
if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    control_path="$XDG_RUNTIME_DIR/handfast/handfastd.sock"
else
    control_path="$data_dir/handfastd.sock"
fi

printf '\nHandfast share interop run\n'
printf '  device id:      %s\n' "$device_id"
printf '  certificate:    %s\n' "${fingerprint:-unknown}"
printf '  TCP port:       %s\n' "${tcp_port:-unknown}"
printf '  control path:   %s\n' "$control_path"
printf '  data dir:       %s\n' "$data_dir"
printf '  log:            %s\n' "$log"
if [ "$incoming_supported" -eq 1 ]; then
    printf '  incoming dir:   %s\n' "$incoming_dir"
    incoming_note="Received files appear in $incoming_dir with no .part file left."
else
    printf '  incoming dir:   blocked, this handfastd has no --incoming-dir\n'
    incoming_note="This build has no --incoming-dir, so the inbound file checks are blocked. Record them as blocked, not passed."
fi

cat <<EOF

Local identity packet:

$identity

Pair both peers before this run. The link checklists are in docs/INTEROP.md.
Watch the log in a second terminal:

  tail -f "$log"

Inbound from Android (KDE Connect app, Share to Handfast):
  1. Share a photo, then text, then an https URL.
  2. Share an empty file and a file larger than 100 MiB.
  3. Cancel one transfer, then share two files at once.
  Expect share.request with the name, size and payload port, then
  ShareProgress and ShareCompleted. Text and URL lines show a length only.
  $incoming_note

Inbound from the desktop KDE Connect client:
  1. Share a file, then text, then an https URL.
  2. Queue a second file behind a large first file.
  Expect the same log lines, both files in order, and no partial file.
  The payload TLS version is TLS 1.2 for Android and TLS 1.3 for the desktop.

Outbound to a paired peer:

  "$hfctl" share --device <peer-id> --data-dir "$data_dir" <file>

  Expect "sent <n> bytes to <peer-id>" and exit 0. A remote or IO error
  exits 1, a usage error exits 2.

EOF

printf 'Press Enter to stop handfastd: '
read -r _ || true
cleanup

printf '\nhandfastd stopped.\n'
printf '  log:            %s\n' "$log"
printf '  data dir:       %s\n' "$data_dir"
if [ "$incoming_supported" -eq 1 ]; then
    printf '  incoming dir:   %s\n' "$incoming_dir"
    printf '\nReceived files:\n'
    ls -la "$incoming_dir" 2>/dev/null || true
fi
printf '\nPacket capture for a failed step:\n'
printf "  sudo tcpdump -i any -s 0 -w %s/share.pcap 'tcp portrange 1716-1764'\n" "$run_dir"
printf 'Do not attach state.db, it holds the private key.\n'
printf 'Remove the run dir when the evidence is filed: rm -rf %s\n' "$run_dir"
