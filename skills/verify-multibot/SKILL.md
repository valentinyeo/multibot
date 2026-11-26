---
name: verify-multibot
description: >-
  End-to-end verification skill for Multibot (Rust + GPUI Kit desktop + multibotd daemon).
  Maps every changed crate/file to automated unit tests, headless GPUI Kit UI journeys,
  and DXGI non-NVIDIA GPU checks.
---

# verify-multibot

Run `./scripts/verify.sh` from the repository root after changes. Every changed file must map to at least one automated verification check below; unmapped changes fail verification.

## Feature Map

| Area / Paths | Verification Journeys |
| --- | --- |
| `crates/multibot-gpu/**`, `patches/gpui-pre-windows/**` | `cargo test -p multibot-gpu` — proves NVIDIA (`0x10DE`) is skipped even at index 0, NVIDIA-only hosts fall back to WARP software rendering, explicit LUID overrides work, and DXGI report parsing roundtrips cleanly. |
| `crates/multibot-keys/**`, `docs/keyboard-shortcuts.md` | `cargo test -p multibot-keys` — proves zero duplicate IDs or conflicting chords and verifies canonical legacy shortcuts (`Ctrl+K`, `Ctrl+Shift+K`, `Alt+I`, `Ctrl+Shift+I`, `Ctrl+Shift+B`, `Ctrl+,`, `F1`) are preserved. |
| `crates/multibot-proto/**` | `cargo test -p multibot-proto` — proves typed JSONL client/server wire framing roundtrips. |
| `crates/multibot-core/**` | `cargo test -p multibot-core` — proves `%APPDATA%\Multibot` settings isolation and read-only `%APPDATA%\ZigShell` host import. |
| `crates/multibotd/**` | `cargo test -p multibotd` + release binary smoke test (`hello` -> `hello_ack`, `ping` -> `pong`) — proves Claude/Codex/Antigravity discovery, JSONL parsing, and stdio protocol health. |
| `crates/multibot-desktop/**` | `cargo test -p multibot-desktop` (`#[gpui_kit::test]` headless UI journeys) — renders obsidian dark shell, opens/switches GPU Settings, opens `Ctrl+K` Command Palette, navigates to `Alt+I` Inbox & filters tabs, opens/closes `F1` Shortcuts overlay, toggles sidebar. |
| `.github/workflows/**`, `Cargo.toml` | `cargo test --workspace` + release build check (`cargo build --release -p multibotd`). |

## Hard Invariants (Never Regress)
1. **0% NVIDIA GPU by default on Windows:** Never invoke `D3D11CreateDevice` on `VendorId == 0x10DE` unless explicitly overridden by LUID in settings; fall back to WARP software rendering if no non-NVIDIA hardware GPU succeeds.
2. **Parallel isolation:** Never write to `%APPDATA%\ZigShell` or overwrite legacy VPS Python scripts.
3. **Keyboard shortcuts preserved:** Any new user-facing command updates `crates/multibot-keys/src/lib.rs` and preserves `docs/keyboard-shortcuts.md`.
4. **Zero idle spin:** Never add continuous 60 FPS background redraw loops when idle.
