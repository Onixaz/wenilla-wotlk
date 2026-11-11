#!/usr/bin/env bash
# The browser build's clock lint: clippy on wasm32 with `scripts/wasm-lint/clippy.toml`, which
# refuses std's `Instant`, `SystemTime`, `thread::sleep` and `thread::spawn`. Each compiles on
# wasm32 and panics when called, so `cargo check --target wasm32`, the native clippy and every test
# pass a client that dies at boot with `RuntimeError: unreachable`. On wasm32 Bevy's and
# `web-time`'s `Instant` are their own types, so the lint sees only what the page would run.
# Only that lint is denied; other wasm warnings stay warnings. Needs scripts/web-setup.sh.
#
#   scripts/wasm-lint.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

export WASI_SDK="${WASI_SDK:-$(pwd)/tools/wasi-sdk}"
if ! rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown \
    || [ ! -f "$WASI_SDK/share/wasi-sysroot/lib/wasm32-wasip1/libsetjmp.a" ]; then
    echo "wasm-lint: the wasm32 toolchain is missing; run scripts/web-setup.sh" >&2
    exit 1
fi

CLIPPY_CONF_DIR="$(pwd)/scripts/wasm-lint" exec cargo clippy \
    --target wasm32-unknown-unknown -p wenilla --no-default-features --features webgpu \
    -- -A warnings -D clippy::disallowed_types -D clippy::disallowed_methods
