#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

case "${1:-}" in
    ""|--serve) ;;
    *)
        echo "Usage: $0 [--serve]" >&2
        exit 2
        ;;
esac

if ! command -v wasm-bindgen >/dev/null 2>&1; then
    echo "wasm-bindgen is missing. Install it with: cargo install wasm-bindgen-cli --version 0.2.129 --locked" >&2
    exit 1
fi

if [[ "${1:-}" == --serve ]] && ! command -v caddy >/dev/null 2>&1; then
    echo "Caddy is missing. Install Caddy to use --serve." >&2
    exit 1
fi

wasm_target=wasm32-unknown-unknown
if ! rustup target list --installed | grep -qx "$wasm_target"; then
    rustup target add "$wasm_target"
fi

cargo build --release --locked --target "$wasm_target" --lib
mkdir -p dist/pkg
wasm-bindgen --target web --out-dir dist/pkg "target/$wasm_target/release/stitchviewer.wasm"
cp index.html dist/index.html
mkdir -p dist/assets
cp assets/stitchviewer.svg dist/assets/stitchviewer.svg
echo "Built: dist/index.html"

if [[ "${1:-}" == --serve ]]; then
    caddy run --config Caddyfile
fi
