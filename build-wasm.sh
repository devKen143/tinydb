#!/usr/bin/env bash
# Compiles the engine (in-memory storage) to WebAssembly for the browser SQL console.
set -e
cd "$(dirname "$0")"
cargo build --lib --release --target wasm32-unknown-unknown
T="${CARGO_TARGET_DIR:-target}"
mkdir -p web/public
cp "$T/wasm32-unknown-unknown/release/tinydb.wasm" web/public/tinydb.wasm
echo "wrote web/public/tinydb.wasm ($(wc -c < web/public/tinydb.wasm) bytes)"
