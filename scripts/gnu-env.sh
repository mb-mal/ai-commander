#!/bin/bash
# Build environment for Windows-without-MSVC machines: pins the fully
# self-contained GNU toolchain via RUSTUP_TOOLCHAIN (rust-toolchain.toml
# stays cross-platform "stable") and provides dlltool.exe/as.exe (needed by
# rustc for raw-dylib import libs on x86_64-pc-windows-gnu).
# Source it before cargo commands:
#   source scripts/gnu-env.sh && cargo build

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export PATH="$HERE/../tools/mingw-bin:$PATH"
export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable-x86_64-pc-windows-gnu}"
