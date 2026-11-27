#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

echo "==> Checking Hard Invariant #1 (Windows DXGI patch graph, no gpui-fast, no dGPU exports)..."
win_tree="$(cargo tree -p multibot-desktop --target x86_64-pc-windows-msvc -f "{p}")"
if grep -qi 'gpui-fast' <<< "${win_tree}"; then
    echo "FAIL: Forbidden 'gpui-fast' detected in multibot-desktop Windows dependency tree!" >&2
    exit 1
fi
if ! grep -q 'gpui-pre-windows.*patches/gpui-pre-windows' <<< "${win_tree}"; then
    echo "FAIL: Expected local patches/gpui-pre-windows in multibot-desktop Windows dependency tree!" >&2
    exit 1
fi
if grep -rnE 'NvOptimusEnablement|AmdPowerXpress' crates/ patches/; then
    echo "FAIL: Forbidden dGPU export symbol (NvOptimusEnablement/AmdPowerXpress) detected!" >&2
    exit 1
fi

echo "==> Running full workspace tests (unit + headless GPUI Kit UI journeys)..."
cargo test --workspace

echo "==> Building release multibotd & verifying --version and --stdio protocol..."
cargo build --release -p multibotd

ver_out="$(./target/release/multibotd --version)"
if ! grep -Eq '^multibotd [0-9]+\.[0-9]+\.[0-9]+' <<< "${ver_out}"; then
    echo "FAIL: Unexpected multibotd --version output: '${ver_out}'" >&2
    exit 1
fi

out="$(printf '{"op":"hello","client_version":"0.1.0"}\n{"op":"ping","nonce":1}\n' | ./target/release/multibotd --stdio)"
grep -q '"hello_ack"' <<< "${out}"
grep -Eq '"binary_hash":"[0-9a-f]{12}"' <<< "${out}"
grep -q '"pong"' <<< "${out}"

echo "==> PASS: all Multibot verification journeys succeeded."
