---
name: verify-multibot
description: >-
  End-to-end verification skill for Multibot (7-crate Rust + GPUI Kit desktop + multibotd daemon).
  Maps every changed crate/file to automated unit tests, headless GPUI Kit UI journeys,
  DXGI non-NVIDIA GPU checks, and the 4-tier opaque-box E2E suite.
---

# verify-multibot

Run `./scripts/verify.sh` from the repository root after changes. For full 4-tier opaque-box E2E coverage across all 38 features and 4 Hard Invariants, also run `./e2e/run_e2e.sh --tier all --no-build`. Every changed file must map to at least one automated verification check below; unmapped changes fail verification.

## Feature Map

| Area / Paths | Verification Journeys |
| --- | --- |
| `crates/multibot-proto/**` | `cargo test -p multibot-proto` — proves typed additive JSONL wire framing (`ClientRequest`/`ServerEvent`, rich `MessageBlock`/`DiffBlock`/`TodoItem`/`SlashCommandItem`/`SearchHit`, `build_version()`, `binary_hash`) and Slice 1 backward compatibility. |
| `crates/multibot-gpu/**`, `patches/gpui-pre-windows/**` | `cargo test -p multibot-gpu` + Windows `cargo tree` check (zero `gpui-fast`, local `patches/gpui-pre-windows`) + zero `NvOptimusEnablement`/`AmdPowerXpress` scan — proves NVIDIA (`0x10DE`) is skipped even at index 0, NVIDIA-only hosts fall back to WARP software rendering, explicit LUID overrides work, and DXGI report parsing roundtrips cleanly. |
| `crates/multibot-keys/**`, `docs/keyboard-shortcuts.md` | `cargo test -p multibot-keys` — proves zero duplicate IDs or conflicting chords, zero `Ctrl+Alt+<key>` AltGr conflicts, Phase 2 reserved chord isolation, and preservation of all canonical shortcuts (`Ctrl+K`, `Ctrl+Shift+K`, `Alt+I`, `Ctrl+Shift+I`, `Ctrl+Shift+B`, `Alt+1..9`, `Alt+S`, `Ctrl+G`, `F2`, `Ctrl+Shift+D`, `Ctrl+F`, `Ctrl+/`, `Ctrl+,`, `F1`). |
| `crates/multibot-updater/**` | `cargo test -p multibot-updater` — proves GitHub Releases ETag polling (`304 Not Modified` zero-redraw, `403` rate-limit backoff), strict SemVer + draft/prerelease/`rejected.txt` filtering, HTTPS host allowlist, SHA-256 & zip-slip (`../`) verification, 256 MiB cap, and Windows locked-exe atomic swap + rollback + `--update-ready` handshake + `--gpu=...` arg preservation + `.old` cleanup. |
| `crates/multibot-core/**` | `cargo test -p multibot-core` — proves `%APPDATA%\Multibot` isolation, strictly read-only `%APPDATA%\ZigShell` import (hosts + optional `IdentityFile` + workspaces + favorites with symlink/cap guards), SSH `-i <path>` pass-through & injection rejection, version+SHA-12 atomic remote `multibotd` deploy & 1-shot `HelloAck` upgrade, workspaces/favorites/groups, two-tier `<16ms` LRU chat cache (`chatcache/`), and persisted offline outbox (`chat-outbox.json`). |
| `crates/multibotd/**` | `cargo test -p multibotd` + release binary check (`multibotd --version` + `--stdio` `hello`→`hello_ack` with 12-hex `binary_hash`, `ping`→`pong`) — proves Claude Code, Codex, and Antigravity (`agy` + `~/.multibot/history/`) rich parsing (tools, diffs, todos, prompts, base64 scrubbing), multi-session/directory `inotify` streaming, semantic working/unread tracking, interrupt/steer/cancel, spawn/rename/archive/unarchive, pagination, search, and slash-command discovery. |
| `crates/multibot-desktop/**` | `cargo test -p multibot-desktop` (`#[gpui_kit::test]` headless UI journeys) — renders obsidian dark OLED shell, verifies multi-window `SharedAppStore`, GPU Settings & read-only ZigShell import, Sidebar (workspaces, favorites, groups, 3-state `Ctrl+Shift+B`, update card with zero duration copy), Cross-Workspace `Alt+I` Inbox, ranked `Ctrl+K` Palette, Rich Chat View (`syntect`, expandable tables, diffs/tools/todos, option cards, quote/reply, reactions, slash picker), and `F1` Shortcuts overlay. |
| `.github/workflows/ci.yml`, `Cargo.toml`, `scripts/verify.sh`, `skills/verify-multibot/SKILL.md` | `./scripts/verify.sh` (`cargo test --workspace`, release `multibotd` build + CLI/stdio check, Hard Invariant #1 `cargo tree` & symbol scan). |
| `e2e/**`, `TEST_INFRA.md`, `TEST_READY.md` | `./e2e/run_e2e.sh --tier all --no-build` (86 opaque-box tests across Tiers 1–4 covering all 4 Hard Invariants and 38 features; use `--baseline-only` for fast 17-test baseline check). |

## Hard Invariants (Never Regress)
1. **0% NVIDIA GPU by default on Windows:** Never invoke `D3D11CreateDevice` on `VendorId == 0x10DE` unless explicitly overridden by LUID in settings; fall back to WARP software rendering if no non-NVIDIA hardware GPU succeeds; NEVER enable `gpui-kit`'s `gpui-fast` feature; NEVER export `NvOptimusEnablement` or `AmdPowerXpress`.
2. **Parallel isolation:** Never write to `%APPDATA%\ZigShell`, legacy Multiprompt repos, or legacy VPS Python scripts; keep all desktop data under `%APPDATA%\Multibot` and VPS daemon data under `~/.multibot/`.
3. **Keyboard shortcuts preserved:** Any new user-facing command updates `crates/multibot-keys/src/lib.rs`, preserves `docs/keyboard-shortcuts.md`, and never binds `Ctrl+Alt+<key>` (AltGr conflict).
4. **Zero idle spin & clean chrome:** Never add continuous 60 FPS background redraw loops when idle; zero per-message `"Sending..."`/ETA chrome; zero time/duration wording in update UI copy.
