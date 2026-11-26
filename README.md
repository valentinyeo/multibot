# Multibot

Lean, native Rust + GPUI desktop workspace for running and monitoring multiple AI agents across local and remote VPS hosts, paired with a tiny compiled Linux daemon (`multibotd`).

## Design Pillars
- **0% NVIDIA GPU by default on Windows:** Patched `gpui-pre-windows` DXGI adapter selection picks the integrated / low-power GPU (`DXGI_GPU_PREFERENCE_MINIMUM_POWER`), explicitly skips NVIDIA (`0x10DE`), never falls back to NVIDIA (uses Windows WARP software rendering if iGPU fails), and exposes an in-app GPU picker + active-GPU status badge.
- **Lean footprint & low latency:** Single Rust process across multiple windows, `gpui-kit` compiled with `default-features = false`, demand-driven rendering, and a static Linux `multibotd` daemon using Linux `inotify` instead of Python polling.
- **100% parallel coexistence with legacy Multiprompt:** Uses `%APPDATA%\Multibot` and `~/.multibot/multibotd` — never modifies `%APPDATA%\ZigShell` or existing VPS scripts.

## Workspace Crates
- `crates/multibot-proto` — Shared typed JSONL protocol between desktop and `multibotd`.
- `crates/multibot-gpu` — Cross-platform GPU selection policy, NVIDIA exclusion rules, and DXGI report parsing.
- `crates/multibot-core` — Settings (`%APPDATA%\Multibot`), read-only `%APPDATA%\ZigShell` importer, SSH + auto-deploy client, and footprint telemetry.
- `crates/multibotd` — Linux VPS daemon discovering and streaming Claude Code, Antigravity (`agy`), and Codex sessions over stdio/SSH.
- `crates/multibot-desktop` — Windows/Linux GPUI Kit desktop application.
- `patches/gpui-pre-windows` — Minimal DXGI GPU-selection patch on `gpui-pre-windows 0.3.14`.

## Quick Commands
```bash
cargo test --workspace
cargo build --release -p multibotd
cargo build --release -p multibot-desktop
```
