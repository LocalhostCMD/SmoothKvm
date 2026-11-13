#!/usr/bin/env bash
# Writes dist/latest.json (version, file names, SHA-256) and signs it with the SmoothKvm update key
# (Ed25519, private key kept outside the repository). The app installs an update only if this signature
# verifies against the public key built into it.
#
#   packaging/make-manifest.sh <version> <windows-installer.exe> <linux.deb> ["release notes"]
#
# Key: $SMOOTHKVM_UPDATE_KEY or ~/.smoothkvm-release/update-signing-key.pem
set -euo pipefail
V="$1"; WIN="$2"; DEB="$3"; NOTES="${4:-}"
KEY="${SMOOTHKVM_UPDATE_KEY:-$HOME/.smoothkvm-release/update-signing-key.pem}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/dist"
mkdir -p "$OUT"
[ -f "$KEY" ] || { echo "signing key not found: $KEY" >&2; exit 1; }
EXPECT="80b11525677c912fc3adab838bc46ebea2d5b02b867f7952c63cb541db56ba44"
GOT="$(openssl pkey -in "$KEY" -pubout -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n')"
[ "$GOT" = "$EXPECT" ] || { echo "this key does not match the public key built into the app" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
size() { wc -c < "$1" | tr -d ' '; }
esc() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g' | tr '\n' ' '; }
cat > "$OUT/latest.json" <<EOF
{
  "version": "$V",
  "notes": "$(esc "$NOTES")",
  "windows": {"name": "$(basename "$WIN")", "sha256": "$(sha "$WIN")", "size": $(size "$WIN")},
  "linux": {"name": "$(basename "$DEB")", "sha256": "$(sha "$DEB")", "size": $(size "$DEB")}
}
EOF
openssl pkeyutl -sign -inkey "$KEY" -rawin -in "$OUT/latest.json" -out "$OUT/latest.json.sig.bin"
od -An -tx1 "$OUT/latest.json.sig.bin" | tr -d ' \n' > "$OUT/latest.json.sig"
rm -f "$OUT/latest.json.sig.bin"
echo "wrote $OUT/latest.json and latest.json.sig"
cat "$OUT/latest.json"
