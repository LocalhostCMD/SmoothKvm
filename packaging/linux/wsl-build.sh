#!/usr/bin/env bash
# Helper for building the .deb from Windows through WSL (copies the source to the fast Linux disk first).
set -euo pipefail
SRC="${1:-/mnt/c/Users/Server/Documents/Softwarekvm}"
VERSION="${2:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$SRC/Cargo.toml" | head -n1 | tr -d '\r')}"
rm -rf ~/skvm && mkdir -p ~/skvm
(cd "$SRC" && tar --exclude=./target --exclude='./target-*' --exclude=./dist --exclude=./.git -cf - .) | tar -xf - -C ~/skvm
cd ~/skvm
find packaging/linux -type f \( -name '*.sh' -o -path '*debian/*' -o -name '*.service' -o -name '*.path' -o -name '*.rules' -o -name '*.desktop' -o -name '*.svg' \) -exec sed -i 's/\r$//' {} +
chmod +x packaging/linux/*.sh
source ~/.cargo/env
./packaging/linux/build-deb.sh "$VERSION"
mkdir -p "$SRC/dist"
cp dist/smoothkvm_${VERSION}_amd64.deb "$SRC/dist/"
