#!/usr/bin/env bash
set -euo pipefail

echo "==> Running full workspace tests (unit + headless GPUI Kit UI journeys)..."
cargo test --workspace

echo "==> Building release multibotd & running stdio protocol smoke test..."
cargo build --release -p multibotd
out=$(printf '{"op":"hello","client_version":"0.1.0"}\n{"op":"ping","nonce":1}\n' | ./target/release/multibotd --stdio)
echo "$out" | grep -q '"hello_ack"'
echo "$out" | grep -q '"pong"'

echo "==> PASS: all Multibot verification journeys succeeded."
