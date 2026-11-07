#!/bin/bash
# Build environment for the GNU toolchain: provides dlltool.exe (needed by
# rustc for raw-dylib import libs on x86_64-pc-windows-gnu).
# Source it before cargo commands:
#   source scripts/gnu-env.sh && cargo build

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export PATH="$HERE/../tools/mingw-bin:$PATH"
