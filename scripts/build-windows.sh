#!/usr/bin/env bash
set -euo pipefail

windows_target=x86_64-pc-windows-gnu

if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    echo "x86_64-w64-mingw32-gcc is missing. Install the MinGW-w64 toolchain." >&2
    exit 1
fi

if ! rustup target list --installed | grep -qx "$windows_target"; then
    rustup target add "$windows_target"
fi

cargo build --release --locked --target "$windows_target" --bin stitchviewer
echo "Built: target/$windows_target/release/stitchviewer.exe"
