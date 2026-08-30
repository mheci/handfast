#!/usr/bin/env bash
# package-rpm.sh <version> <binroot> <outdir>
#
# Assemble a Fedora .rpm from prebuilt Handfast binaries.
#   <version>  release version, e.g. 0.1.1 (no leading v)
#   <binroot>  directory containing handfastd / hfctl / handfast-gui
#   <outdir>   destination directory for handfast-<ver>-1.x86_64.rpm
#
# Run from the repo root. Requires rpmbuild (Fedora: dnf install rpm-build;
# Debian/Ubuntu: apt install rpm). Shared by the Release workflow (release
# binaries) and CI packaging-smoke (debug binaries).
set -euo pipefail

VER="$1"
ROOT="$2"
OUT="$3"
SPEC="${4:-packaging/fedora/handfast.spec}"

if [ ! -x "$ROOT/handfastd" ] || [ ! -x "$ROOT/hfctl" ] || [ ! -x "$ROOT/handfast-gui" ]; then
    echo "error: expected handfastd/hfctl/handfast-gui under $ROOT" >&2
    exit 1
fi
command -v rpmbuild >/dev/null 2>&1 || {
    echo "error: rpmbuild not found (install 'rpm-build' on Fedora or 'rpm' on Debian/Ubuntu)" >&2
    exit 1
}

mkdir -p "$OUT"
TOPDIR="$(mktemp -d)"
trap 'rm -rf "$TOPDIR"' EXIT
mkdir -p "$TOPDIR"/{SPECS,BUILD,RPMS,SOURCES,BUILDROOT,db}

# Pre-stage every payload file under the buildroot that %install copies from.
STAGE="$TOPDIR/stage"
mkdir -p "$STAGE/usr/bin" \
    "$STAGE/usr/lib/systemd/user" \
    "$STAGE/usr/share/applications" \
    "$STAGE/usr/share/icons/hicolor/scalable/apps" \
    "$STAGE/usr/share/licenses/handfast" \
    "$STAGE/usr/share/bash-completion/completions" \
    "$STAGE/usr/share/zsh/site-functions" \
    "$STAGE/usr/share/fish/vendor_completions.d"

install -Dm755 "$ROOT/handfastd" "$ROOT/hfctl" "$ROOT/handfast-gui" -t "$STAGE/usr/bin/"
install -Dm644 packaging/systemd/handfast.service "$STAGE/usr/lib/systemd/user/handfast.service"
install -Dm644 packaging/desktop/dev.handfast.Gui.desktop "$STAGE/usr/share/applications/dev.handfast.Gui.desktop"
install -Dm644 packaging/icons/handfast.svg "$STAGE/usr/share/icons/hicolor/scalable/apps/handfast.svg"
install -Dm644 LICENSE-MIT "$STAGE/usr/share/licenses/handfast/LICENSE-MIT"

"$ROOT/hfctl" completions bash > "$STAGE/usr/share/bash-completion/completions/hfctl"
"$ROOT/hfctl" completions zsh  > "$STAGE/usr/share/zsh/site-functions/_hfctl"
"$ROOT/hfctl" completions fish > "$STAGE/usr/share/fish/vendor_completions.d/hfctl.fish"

sed "s/^Version:.*/Version: ${VER}/" "$SPEC" > "$TOPDIR/SPECS/handfast.spec"

rpmbuild -bb \
    --define "_topdir $TOPDIR" \
    --define "_dbpath $TOPDIR/db" \
    --define "debug_package %{nil}" \
    --define "__os_install_post %{nil}" \
    --define "_build_id_links none" \
    --define "binroot $STAGE" \
    "$TOPDIR/SPECS/handfast.spec" >/dev/null

cp "$TOPDIR/RPMS/x86_64/handfast-${VER}-1.x86_64.rpm" "$OUT/"
echo "built $OUT/handfast-${VER}-1.x86_64.rpm"
