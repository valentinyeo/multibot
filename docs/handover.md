# Multibot Handover (VPS Continuation)

**Snapshot commit:** `42a4e7a` on `https://github.com/valentinyeo/multibot` (`main`)
**Status:** Verified green (**383/383 Rust workspace tests**, **86/86 E2E tests**, **0 warnings**, `./scripts/verify.sh` PASS). Already verified on Valentin's 2-GPU Windows laptop (**0% NVIDIA**, locked to `Intel(R) Arc(TM) Pro Graphics`, **0.0–0.1% idle CPU**).

## What Is Already Built & Shipped (Slices 0–4 + Auto-Updater)
- **Workspace crates:**
  - `crates/multibot-proto` — Shared typed JSONL protocol between desktop and `multibotd`.
  - `crates/multibot-gpu` + `patches/gpui-pre-windows` — Windows DXGI iGPU-only adapter selector (skips NVIDIA `0x10DE`, WARP fallback, GPU picker & active-GPU status card).
  - `crates/multibot-keys` — Central registry of 58 preserved keyboard shortcuts (`docs/keyboard-shortcuts.md`).
  - `crates/multibot-updater` + `crates/multibot-desktop/src/update_ui.rs` + `.github/workflows/ci.yml` — In-app auto-updater (background check, Update Card + `Ctrl+K` commands, SHA-256 verify, atomic Windows `.exe` + `multibotd` swap, `.old` cleanup, `--gpu` preservation) + automated GitHub Releases publishing on push to `main`.
  - `crates/multibot-core` — `%APPDATA%\Multibot` settings, read-only `%APPDATA%\ZigShell` importer (hosts, workspaces, favorites, AND `IdentityFile` / `ssh -i`), SSH + auto-deploy client, `<16 ms` local chat cache (`cache.rs`, ~2–37 µs switch), offline outbox (`outbox.rs`), workspaces store (`workspaces.rs`), and single-PID footprint sampler.
  - `crates/multibotd` — Compiled Linux VPS daemon (~3.5 MB RSS, strictly `<10 MB` under 20 MiB torture loads): Claude Code, Antigravity (`agy`), and Codex discovery, rich tool/diff/todo/question parsing, streaming bounded base64 scrubber, `inotify` live tailing, `SpawnSession`, steering/interrupt, rename, archive, and history search.
  - `crates/multibot-desktop` — Single-process multi-window GPUI Kit app with Muse/Grok obsidian dark `18px` rounded UI, Sidebar/Workspaces (`Alt+1..9`), Favorites (`Alt+S`), Cross-workspace Inbox (`Alt+I`/`Ctrl+Shift+I`), ranked `Ctrl+K` Command Palette, `F1` Shortcuts overlay, `F2` inline rename, and Rich Chat View (Markdown, code blocks, diffs, expandable tables, option cards, replies/reactions, image paste/drop, `/` slash picker).
  - `e2e/` + `skills/verify-multibot/SKILL.md` + `scripts/verify.sh` — 86-test 4-tier automated E2E suite + `pstack`-compatible verification skill.

## Remaining Roadmap (Pick Up Here on VPS)
1. **First check:** Confirm the `42a4e7a` GitHub Release (`https://github.com/valentinyeo/multibot/releases`) built green on Windows and test live VPS connect + auto-update on Valentin's Windows laptop (fix any real-world Windows/SSH/UI rough edges directly).
2. **Slice 5 — Built-in Terminal & Computer View:**
   - Native GPU terminal toggle (`Alt+C`) shipped in `crates/multibot-desktop/src/terminal_pane.rs`: lazy `portable-pty` + `alacritty_terminal` grid, `ssh -tt` on the active host with a single-quoted safe `cd`, local login shell when offline, obsidian header with Clear / Reconnect / Close. Zero PTY, zero child and zero polling loop while closed.
   - Lazy-loaded GPUI Kit WebView2 Computer View (0 MB RAM when closed).
3. **Slice 6 — Docked Chrome/Edge Browser Pane & Extras:**
   - Isolated `platform/windows/browser_dock` with its own Chrome profile & native-messaging host (`zsb` bridge, `Alt+A`, `Alt+Q`, `Ctrl+Shift+S`, per-chat auto-switching, idle hibernation).
   - Rooms (multi-bot group chats), Deepgram voice dictation (`Ctrl+Shift+Space`), Reminders & Workspace Snooze (`Ctrl+H`, `Ctrl+Shift+Y`), plus Pi & Cursor providers.
4. **Phase 3 — Android Migration:** Point `valentinyeo/multiprompt-android` at `multibot-proto` / `multibotd`, then retire legacy `multiprompt`.

## VPS Quickstart
```bash
git clone https://github.com/valentinyeo/multibot.git
cd multibot
# Linux GPUI dev libs (Ubuntu/Debian):
sudo apt-get update && sudo apt-get install -y libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev libfontconfig1-dev libwayland-dev pkg-config libssl-dev
./scripts/verify.sh
```
