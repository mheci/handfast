#!/usr/bin/env bash
# Build a native .deb from target/release/handfastd.
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

install -d "$root/DEBIAN" "$root/usr/bin" "$root/usr/share/doc/handfastd"
install -m 0755 target/release/handfastd "$root/usr/bin/handfastd"
install -m 0644 README.md "$root/usr/share/doc/handfastd/README.md"
install -m 0644 LICENSE-MIT "$root/usr/share/doc/handfastd/copyright"

cat > "$root/DEBIAN/control" <<EOF
Package: handfastd
Version: ${version}
Section: net
Priority: optional
Architecture: ${arch}
Depends: libc6 (>= ${glibc}), libgcc-s1
Maintainer: mheci <274998646+mheci@users.noreply.github.com>
Homepage: https://github.com/mheci/handfast
Description: KDE Connect compatible daemon
 Handfast is a clean-room, Rust-native KDE Connect implementation.
EOF

mkdir -p "$out"
dpkg-deb --build --root-owner-group "$root" "$out/handfastd_${version}_${label}_${arch}.deb"
