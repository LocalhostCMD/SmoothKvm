#!/usr/bin/env bash
# Builds dist/smoothkvm_<version>_amd64.deb from a release build. Run from anywhere on Ubuntu/Debian.
set -euo pipefail
VERSION="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$(dirname "$0")/../../Cargo.toml" | head -n1)}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
cargo build --release -p kvmd
PKG="$ROOT/dist/deb-root"
rm -rf "$PKG"
mkdir -p "$PKG/DEBIAN" "$PKG/usr/bin" "$PKG/usr/share/applications" "$PKG/usr/share/pixmaps" \
         "$PKG/lib/systemd/system" "$PKG/lib/udev/rules.d" "$PKG/etc/modules-load.d"
install -m755 target/release/SmoothKvm "$PKG/usr/bin/SmoothKvm"
ln -s SmoothKvm "$PKG/usr/bin/smoothkvm"
install -m644 packaging/linux/smoothkvm.service "$PKG/lib/systemd/system/smoothkvm.service"
install -m644 packaging/linux/smoothkvm-helper.service "$PKG/lib/systemd/system/smoothkvm-helper.service"
install -m644 packaging/linux/smoothkvm-helper.path "$PKG/lib/systemd/system/smoothkvm-helper.path"
install -m644 packaging/linux/smoothkvm-terminal.desktop "$PKG/usr/share/applications/smoothkvm-terminal.desktop"
install -m644 packaging/linux/99-smoothkvm-uinput.rules "$PKG/lib/udev/rules.d/99-smoothkvm-uinput.rules"
install -m644 packaging/linux/smoothkvm.desktop "$PKG/usr/share/applications/smoothkvm.desktop"
install -m644 packaging/linux/smoothkvm.svg "$PKG/usr/share/pixmaps/smoothkvm.svg"
echo uinput > "$PKG/etc/modules-load.d/smoothkvm.conf"
sed "s/@VERSION@/$VERSION/" packaging/linux/debian/control > "$PKG/DEBIAN/control"
for s in postinst prerm postrm; do install -m755 "packaging/linux/debian/$s" "$PKG/DEBIAN/$s"; done
dpkg-deb --build --root-owner-group "$PKG" "$ROOT/dist/smoothkvm_${VERSION}_amd64.deb"
echo "built dist/smoothkvm_${VERSION}_amd64.deb"
