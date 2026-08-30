#!/usr/bin/env bash
# package-deb.sh <version> <binroot> <outdir>
#
# Assemble a Debian/Ubuntu .deb from prebuilt Handfast binaries.
#   <version>  release version, e.g. 0.1.1 (no leading v)
#   <binroot>  directory containing handfastd / hfctl / handfast-gui
#   <outdir>   destination directory for handfast_<ver>-1_amd64.deb
#
# Run from the repo root (uses packaging/ and LICENSE-MIT). Shared by the
# Release workflow (release binaries) and CI packaging-smoke (debug binaries).
set -euo pipefail

VER="$1"
ROOT="$2"
OUT="$3"

if [ ! -x "$ROOT/handfastd" ] || [ ! -x "$ROOT/hfctl" ] || [ ! -x "$ROOT/handfast-gui" ]; then
    echo "error: expected handfastd/hfctl/handfast-gui under $ROOT" >&2
    exit 1
fi

mkdir -p "$OUT"
PKGDIR="$(mktemp -d)"
trap 'rm -rf "$PKGDIR"' EXIT

mkdir -p "$PKGDIR/DEBIAN" \
    "$PKGDIR/usr/bin" \
    "$PKGDIR/usr/lib/systemd/user" \
    "$PKGDIR/usr/share/applications" \
    "$PKGDIR/usr/share/icons/hicolor/scalable/apps" \
    "$PKGDIR/usr/share/licenses/handfast" \
    "$PKGDIR/usr/share/bash-completion/completions" \
    "$PKGDIR/usr/share/zsh/site-functions" \
    "$PKGDIR/usr/share/fish/vendor_completions.d"

install -Dm755 "$ROOT/handfastd" "$ROOT/hfctl" "$ROOT/handfast-gui" -t "$PKGDIR/usr/bin/"
install -Dm644 packaging/systemd/handfast.service "$PKGDIR/usr/lib/systemd/user/handfast.service"
install -Dm644 packaging/desktop/dev.handfast.Gui.desktop "$PKGDIR/usr/share/applications/dev.handfast.Gui.desktop"
install -Dm644 packaging/icons/handfast.svg "$PKGDIR/usr/share/icons/hicolor/scalable/apps/handfast.svg"
install -Dm644 LICENSE-MIT "$PKGDIR/usr/share/licenses/handfast/LICENSE-MIT"

"$ROOT/hfctl" completions bash > "$PKGDIR/usr/share/bash-completion/completions/hfctl"
"$ROOT/hfctl" completions zsh  > "$PKGDIR/usr/share/zsh/site-functions/_hfctl"
"$ROOT/hfctl" completions fish > "$PKGDIR/usr/share/fish/vendor_completions.d/hfctl.fish"

SIZE="$(du -sb "$PKGDIR" | cut -f1)"
cat > "$PKGDIR/DEBIAN/control" <<EOF
Package: handfast
Version: ${VER}-1
Section: net
Priority: optional
Architecture: amd64
Maintainer: mheci <274998646+mheci@users.noreply.github.com>
Installed-Size: $((SIZE / 1024))
Depends: libc6 (>= 2.31), libgcc-s1, libwayland-client0, libxkbcommon0, libsqlite3-0
Recommends: mesa-vulkan-drivers
Description: Wayland-first KDE Connect-compatible device pairing daemon
 Connect your Android phone to your Linux desktop over your local network -
 no cloud, no account. Handfast speaks the KDE Connect wire protocol and
 pairs with the standard KDE Connect Android app.
EOF

dpkg-deb --build --root-owner-group "$PKGDIR" "$OUT/handfast_${VER}-1_amd64.deb"
echo "built $OUT/handfast_${VER}-1_amd64.deb"
