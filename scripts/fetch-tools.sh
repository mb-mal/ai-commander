#!/bin/bash
# Fetches dlltool.exe + as.exe (needed to link windows-gnu targets) from the
# official w64devkit release into tools/mingw-bin, verifying the SHA-256.
# Run once after cloning, on Windows machines without MSVC Build Tools:
#   bash scripts/fetch-tools.sh

set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST="$HERE/../tools/mingw-bin"
mkdir -p "$DEST"

W64V="${W64DEVKIT_VERSION:-2.11.0}"
SHA256="${W64DEVKIT_SHA256:-54c2bb9962e8a30c8270e34c5efc3d5fee579f53087fa7d435b95fbba2af7b2b}"
URL="https://github.com/skeeto/w64devkit/releases/download/v${W64V}/w64devkit-x64-${W64V}.7z.exe"
TMP="$(mktemp -d)"

echo "downloading w64devkit $W64V …"
curl -fsSL -o "$TMP/w64.exe" "$URL"

echo "verifying sha256 …"
ACTUAL="$(sha256sum "$TMP/w64.exe" | awk '{print $1}')"
if [ "$ACTUAL" != "$SHA256" ]; then
    echo "checksum mismatch!" >&2
    echo "  expected: $SHA256" >&2
    echo "  actual:   $ACTUAL" >&2
    rm -rf "$TMP"
    exit 1
fi

echo "extracting…"
(cd "$TMP" && ./w64.exe -o"unpack" -y >/dev/null)

for f in dlltool.exe as.exe; do
    cp "$TMP/unpack/w64devkit/bin/$f" "$DEST/"
    echo "  + $DEST/$f"
done

rm -rf "$TMP"
echo "done. build with: source scripts/gnu-env.sh && cargo build --release"
