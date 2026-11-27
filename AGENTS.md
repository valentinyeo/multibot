# Multibot — Agent Instructions & Hard Invariants

## Product & User
- **Owner:** Valentin Yeo (product owner — short, plain English updates; bottom line up front; never ask him to merge, review code, or run manual QA unless hardware verification on his 2-GPU Windows laptop is needed).
- **Goal:** Lean, low-RAM, low-latency native Rust + **GPUI Kit** desktop workspace (`multibot.exe`) paired with a tiny compiled Linux VPS daemon (`multibotd`, ~3.5 MB RAM), replacing the legacy Zig/Python `multiprompt` app cleanly in parallel.

## Hard Invariants (NEVER Break)
1. **0% NVIDIA GPU by default on Windows:**
   - Patched DXGI adapter selector lives in `patches/gpui-pre-windows/src/directx_devices.rs` (wired via `[patch.crates-io]` in root `Cargo.toml`) and policy engine in `crates/multibot-gpu/`.
   - Uses `DXGI_GPU_PREFERENCE_MINIMUM_POWER`, explicitly skips NVIDIA (`VendorId == 0x10DE`), **never** falls back to NVIDIA (uses Windows WARP software rendering if iGPU fails), and exposes an in-app GPU picker + active-GPU status card.
   - Do **not** enable `gpui-fast` or drop the `patches/gpui-pre-windows` override.
2. **100% Parallel Coexistence with Legacy `multiprompt`:**
   - Never write to or modify `%APPDATA%\ZigShell`, legacy `valentinyeo/multiprompt`, or legacy VPS Python scripts.
   - Desktop stores data strictly under `%APPDATA%\Multibot` (`~/.config/multibot` on Linux). VPS daemon deploys strictly to `~/.multibot/multibotd`.
   - Importing SSH hosts/workspaces/favorites/keys (`IdentityFile`) from `%APPDATA%\ZigShell` is strictly **read-only**.
3. **100% Keyboard Shortcuts Preserved:**
   - Canonical contract: `docs/keyboard-shortcuts.md`.
   - Single source of truth in Rust: `crates/multibot-keys/src/lib.rs` (wired in `crates/multibot-desktop/src/main.rs`). Never break existing chords (`Ctrl+K`, `Ctrl+Shift+K`, `Alt+I`, `Ctrl+Shift+I`, `Alt+1..9`, `Ctrl+1..9`, `Alt+C`, `Alt+Q`, `Alt+A`, `Alt+S`, `Alt+J/K`, `Alt+Left/Right`, `Ctrl+Shift+B`, `Ctrl+Shift+F`, `Ctrl+Shift+S`, `Ctrl+Shift+Space`, `Ctrl+,`, `Ctrl+N`, `F1`, `F2`, `Esc`).
4. **Low RAM, ~0% Idle CPU/GPU & Lean Footprint:**
   - Single Rust process managing multiple OS windows (`SharedAppStore` in `crates/multibot-desktop/src/state.rs`).
   - Keep `gpui-kit` on `default-features = false` (no unused `gpui-kit-assets`, `tree-sitter-*`, `syntect`, or JS shell unless explicitly needed).
   - Strictly demand-driven rendering — zero 60 FPS idle loops or silent-poll UI redraws.
   - Keep `multibotd` peak RSS `< 10 MB` on VPS.

## Visual Style
- **Muse / Grok Bot obsidian dark OLED theme** (`crates/multibot-desktop/src/theme.rs`):
  - `#0D0D0D` canvas, `#111111` sidebar, `#1A1A1A` cards/modals, `#2B2B2B` user bubbles, `#2A2A2A` hairlines.
  - Generous soft radii: **`18px`** chat bubbles & modals (`RADIUS_BUBBLE`, `RADIUS_MODAL`), **`12px`** sidebar/list rows (`RADIUS_ROW`), **full-pill** (`9999px`) floating header, composer bar, filter tabs, and shortcut chips (`RADIUS_CHIP`).
  - Prefer ready-made **`gpui-kit` / `gpui-component`** widgets over hand-drawn UI.

## Verification & Workflow
- **Release notes:** Add user-facing changes to root `CHANGELOG.md` under `Unreleased`; move them under the published version when releasing. Keep release headings as `##` and each change as a single-line `-` bullet. The desktop embeds this file at compile time for its offline Changelog modal.
- **Fast local verification (~15s on Linux):**
  ```bash
  ./scripts/verify.sh
  ```
- **Full opaque-box E2E suite (86 tests across 4 tiers):**
  ```bash
  ./e2e/run_e2e.sh
  ```
- **CI & Auto-Release (`.github/workflows/ci.yml`):** Every push to `main` builds Linux `multibotd` + Windows `multibot.exe` in parallel and publishes a GitHub Release (`v0.1.<run>`) with `multibot-windows-x64.zip`, `SHA256SUMS.txt`, and `update-manifest.json`. Running Windows apps auto-detect, prompt, verify SHA-256, swap, and relaunch cleanly.
- **Avoid heavy multi-round challenger loops** — iterate directly in small verified commits and let `./scripts/verify.sh` + CI catch regressions cheaply.
