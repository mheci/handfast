#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 4 ]; then
    echo "usage: $0 <version> <glibc-min> <label> <out-dir>" >&2
    exit 2
fi

version=$1
glibc=$2
label=$3
out=$4

arch=$(dpkg --print-architecture)
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT

install -d "$root/DEBIAN" \
    "$root/usr/bin" \
    "$root/usr/lib/systemd/user" \
    "$root/usr/share/doc/handfast"
install -m 0755 target/release/handfast "$root/usr/bin/handfast"
install -m 0644 packaging/linux/handfast.service "$root/usr/lib/systemd/user/handfast.service"
install -m 0644 README.md "$root/usr/share/doc/handfast/README.md"
install -m 0644 LICENSE-MIT "$root/usr/share/doc/handfast/copyright"
install -m 0644 THIRD-PARTY-NOTICES.md "$root/usr/share/doc/handfast/THIRD-PARTY-NOTICES.md"

cat > "$root/DEBIAN/control" <<EOF
Package: handfast
Version: ${version}
Section: net
Priority: optional
Architecture: ${arch}
Depends: libc6 (>= ${glibc}), libgcc-s1
Conflicts: handfastd
Replaces: handfastd
Provides: handfastd
Maintainer: mheci <274998646+mheci@users.noreply.github.com>
Homepage: https://github.com/mheci/handfast
Description: KDE Connect compatible client
 Handfast is a clean-room, Rust-native KDE Connect implementation.
EOF

mkdir -p "$out"
dpkg-deb --build --root-owner-group "$root" "$out/handfast_${version}_${label}_${arch}.deb"
