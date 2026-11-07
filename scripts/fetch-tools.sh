#!/bin/bash
# Fetches dlltool.exe + as.exe (needed to link windows-gnu targets) from the
# official w64devkit release into tools/mingw-bin. Run once after cloning:
#   bash scripts/fetch-tools.sh

set -e
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST="$HERE/../tools/mingw-bin"
mkdir -p "$DEST"

W64V="${W64DEVKIT_VERSION:-2.11.0}"
URL="https://github.com/skeeto/w64devkit/releases/download/v${W64V}/w64devkit-x64-${W64V}.7z.exe"
TMP="$(mktemp -d)"

echo "downloading w64devkit $W64V …"
curl -sL -o "$TMP/w64.exe" "$URL"

echo "extracting…"
(cd "$TMP" && ./w64.exe -o"unpack" -y >/dev/null)

for f in dlltool.exe as.exe; do
    cp "$TMP/unpack/w64devkit/bin/$f" "$DEST/"
    echo "  + $DEST/$f"
done

rm -rf "$TMP"
echo "done. build with: source scripts/gnu-env.sh && cargo build --release"
