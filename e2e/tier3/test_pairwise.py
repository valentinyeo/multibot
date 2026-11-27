"""Tier 3 Pairwise Cross-Feature Combination E2E Tests (T3-01..10)."""

from __future__ import annotations

import json
import os
import re

from e2e.lib import REPO_ROOT
from e2e.lib.cargo_runner import CARGO
from e2e.lib.daemon_driver import DaemonDriver
from e2e.lib.fixture_gen import (
    generate_large_claude_jsonl,
    generate_search_claude_jsonl,
    install_fake_agent_shims,
)
from e2e.lib.runner import e2e_test
from e2e.lib.sandbox import IsolatedHome
from e2e.lib.static_checks import (
    check_dxgi_patch_invariants,
    production_rust_source,
    read_crate_production_sources,
    scan_chat_chrome_violations,
)


@e2e_test(
    id="T3-01",
    tier=3,
    area="pairwise",
    features=("F8", "INV-1"),
    title="Post-update atomic relaunch preserves --gpu=software flag & settings.json GPU policy (0% NVIDIA)",
    baseline=False,
)
def test_pairwise_update_relaunch_preserves_gpu_policy() -> None:
    check_dxgi_patch_invariants(REPO_ROOT)
    upd_prod = read_crate_production_sources(REPO_ROOT / "crates" / "multibot-updater" / "src")
    has_cli_arg_forwarding = (
        "--gpu" in upd_prod
        or (
            "env::args" in upd_prod
            and "preserved_cli_args" in upd_prod
            and "spawn_relaunch" in upd_prod
        )
    )
    assert has_cli_arg_forwarding, (
        "Updater production code must preserve/forward CLI arguments (including --gpu) on relaunch "
        "via explicit --gpu handling or std::env::args() -> preserved_cli_args() -> spawn_relaunch()"
    )
    CARGO.assert_crate_tests_pass(
        "multibot-updater",
        min_expected=8,
        required_any_of=[("relaunch", "gpu", "preserve", "cli_arg", "swap")],
    )
    CARGO.assert_crate_tests_pass("multibot-gpu", min_expected=5)


@e2e_test(
    id="T3-02",
    tier=3,
    area="pairwise",
    features=("F4", "F8", "F10"),
    title="Updated desktop detects HelloAck version+binary_hash mismatch & deploys multibotd-<ver>-<sha12>",
    baseline=False,
)
def test_pairwise_hello_ack_hash_mismatch_triggers_remote_deploy() -> None:
    deploy_rs = REPO_ROOT / "crates" / "multibot-core" / "src" / "daemon_deploy.rs"
    assert deploy_rs.is_file(), "crates/multibot-core/src/daemon_deploy.rs not found"
    deploy_src = production_rust_source(deploy_rs.read_text(encoding="utf-8"))
    assert "multibotd-" in deploy_src and "ln -sf" in deploy_src, (
        "daemon_deploy.rs must deploy version+sha12 path and atomically symlink ~/.multibot/multibotd"
    )

    with IsolatedHome(prefix="mb_t3_02_") as sb:
        with DaemonDriver(sb) as dmn:
            ack = dmn.request({"op": "hello", "client_version": "0.1.99"}, "hello_ack")
            bhash = str(ack.get("binary_hash", ""))
            assert re.match(r"^[0-9a-fA-F]{12,}$", bhash), (
                f"HelloAck must expose >=12-hex binary_hash for mismatch detection, got {ack!r}"
            )

    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="daemon_deploy", min_expected=2
    )


@e2e_test(
    id="T3-03",
    tier=3,
    area="pairwise",
    features=("F14", "F15", "F24"),
    title="Live inotify completion flips working true->false, updates last_assistant_id & promotes Inbox row",
    baseline=False,
)
def test_pairwise_inotify_working_transition_feeds_inbox() -> None:
    inbox_rs = REPO_ROOT / "crates" / "multibot-desktop" / "src" / "inbox.rs"
    assert inbox_rs.is_file(), "crates/multibot-desktop/src/inbox.rs not found"

    with IsolatedHome(prefix="mb_t3_03_") as sb:
        busy_path = sb.stage_fixture(
            "claude/semantic_busy_open_tool.jsonl",
            ".claude/projects/-tmp-t3/busy-inbox.jsonl",
            mtime_offset_sec=-90.0,
        )
        with DaemonDriver(sb) as dmn:
            dmn.request(
                {
                    "op": "watch_session",
                    "session_id": "busy-inbox",
                    "source_path": str(busy_path),
                    "agent": "claude",
                },
                "history",
            )
            with busy_path.open("a", encoding="utf-8") as fh:
                fh.write(
                    json.dumps(
                        {
                            "type": "user",
                            "uuid": "u-t3-res",
                            "timestamp": "2026-11-26T12:00:00Z",
                            "message": {
                                "content": [
                                    {
                                        "type": "tool_result",
                                        "tool_use_id": "tu_open_1",
                                        "is_error": False,
                                        "content": "done",
                                    }
                                ]
                            },
                        }
                    )
                    + "\n"
                )
                fh.write(
                    json.dumps(
                        {
                            "type": "assistant",
                            "uuid": "a-t3-final",
                            "timestamp": "2026-11-26T12:00:01Z",
                            "message": {
                                "content": [{"type": "text", "text": "Ready for your review."}]
                            },
                        }
                    )
                    + "\n"
                )
                fh.flush()
                os.fsync(fh.fileno())

            app = dmn.wait_for(
                "history_append",
                predicate=lambda e: e.get("session_id") == "busy-inbox"
                and "Ready for your review." in json.dumps(e),
                timeout=2.0,
            )
            assert app.get("working") is False, (
                f"Expected history_append working=False on turn completion: {app!r}"
            )

            sess = dmn.request({"op": "list_sessions", "limit": 10}, "sessions")["sessions"]
            row = next((s for s in sess if s["id"] == "busy-inbox"), None)
            assert row is not None and row.get("last_assistant_id") == "a-t3-final", (
                f"Expected SessionSummary.last_assistant_id='a-t3-final' for Inbox sort, got {row!r}"
            )

    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="inbox", min_expected=2)


@e2e_test(
    id="T3-04",
    tier=3,
    area="pairwise",
    features=("F17", "F25", "F26"),
    title="F2 / /rename persists manual title, emits RenameSession to daemon & updates Ctrl+K search",
    baseline=False,
)
def test_pairwise_rename_daemon_and_palette_sync() -> None:
    with IsolatedHome(prefix="mb_t3_04_") as sb:
        c_path = sb.stage_fixture(
            "claude/basic_slice1.jsonl",
            ".claude/projects/-tmp-t3/claude-ren.jsonl",
        )
        with DaemonDriver(sb) as dmn:
            upd = dmn.request(
                {
                    "op": "rename_session",
                    "session_id": "claude-ren",
                    "agent": "claude",
                    "source_path": str(c_path),
                    "title": "GPU Watchdog Audit",
                },
                "session_updated",
            )
            assert upd["session"]["title"] == "GPU Watchdog Audit"

            sess = dmn.request({"op": "list_sessions", "limit": 10}, "sessions")["sessions"]
            renamed = next((s for s in sess if s["id"] == "claude-ren"), None)
            assert renamed is not None and renamed["title"] == "GPU Watchdog Audit"

    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="palette", min_expected=2)


@e2e_test(
    id="T3-05",
    tier=3,
    area="pairwise",
    features=("F20", "F21", "F24"),
    title="Alt+S favorite mirrors to Alt+1 Starred & Inbox Favorites while composing with Ctrl+Shift+U",
    baseline=False,
)
def test_pairwise_favorite_starred_inbox_and_unread_filter() -> None:
    ws_rs = REPO_ROOT / "crates" / "multibot-core" / "src" / "workspaces.rs"
    inbox_rs = REPO_ROOT / "crates" / "multibot-desktop" / "src" / "inbox.rs"
    assert ws_rs.is_file() and inbox_rs.is_file(), "Missing workspaces.rs or inbox.rs"
    CARGO.assert_crate_tests_pass("multibot-core", filter_expr="workspaces", min_expected=3)
    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="inbox", min_expected=2)


@e2e_test(
    id="T3-06",
    tier=3,
    area="pairwise",
    features=("F27", "F20", "F21", "INV-2"),
    title="Read-only ZigShell import migrates hosts + workspaces + favorites while chmod a-w tree stays byte-identical",
    baseline=False,
)
def test_pairwise_readonly_zigshel_migration_with_workspaces() -> None:
    zigshel_rs = REPO_ROOT / "crates" / "multibot-core" / "src" / "zigshel_import.rs"
    z_src = zigshel_rs.read_text(encoding="utf-8")
    assert "workspace" in z_src.lower() and "favorite" in z_src.lower(), (
        "zigshel_import.rs must import workspaces & favorites in addition to SSH hosts"
    )
    with IsolatedHome(prefix="mb_t3_06_") as sb:
        snap = sb.stage_zigshel_fixtures(readonly=True)
        CARGO.assert_crate_tests_pass(
            "multibot-core", filter_expr="zigshel_import", min_expected=2
        )
        sb.assert_unchanged(sb.zigshel_dir, snap)


@e2e_test(
    id="T3-07",
    tier=3,
    area="pairwise",
    features=("F18", "F37"),
    title="Daemon ReadHistory probe_sha256 populates chatcache/<sha256>.jsonl for <16ms warm switch",
    baseline=False,
)
def test_pairwise_daemon_probe_and_two_tier_cache() -> None:
    cache_rs = REPO_ROOT / "crates" / "multibot-core" / "src" / "cache.rs"
    assert cache_rs.is_file(), "crates/multibot-core/src/cache.rs not found"
    with IsolatedHome(prefix="mb_t3_07_") as sb:
        large_path, _ = generate_large_claude_jsonl(
            sb.claude_projects_dir / "-tmp-t3cache" / "claude-cache.jsonl",
            min_bytes=600 * 1024,
        )
        with DaemonDriver(sb) as dmn:
            h = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-cache",
                    "source_path": str(large_path),
                    "agent": "claude",
                    "max_bytes": 524288,
                },
                "history",
            )
            assert h.get("probe_sha256") and h.get("latest_offset") == large_path.stat().st_size, (
                f"Expected non-empty probe_sha256 & latest_offset=={large_path.stat().st_size}, "
                f"got probe={h.get('probe_sha256')!r}, latest_offset={h.get('latest_offset')!r}"
            )

    CARGO.assert_crate_tests_pass("multibot-core", filter_expr="cache", min_expected=3)


@e2e_test(
    id="T3-08",
    tier=3,
    area="pairwise",
    features=("F19", "F33", "F36"),
    title="ListCommands discovers .claude/skills + .agents/skills -> / picker -> Quote/Reply /skill hoist",
    baseline=False,
)
def test_pairwise_list_commands_picker_and_quote_hoist() -> None:
    slash_rs = REPO_ROOT / "crates" / "multibot-desktop" / "src" / "slash_picker.rs"
    chat_rs = REPO_ROOT / "crates" / "multibot-desktop" / "src" / "chat_view.rs"
    assert slash_rs.is_file() and chat_rs.is_file(), "Missing slash_picker.rs or chat_view.rs"

    with IsolatedHome(prefix="mb_t3_08_") as sb:
        sb.stage_fixture(
            "skills/.claude/skills/perf-audit/SKILL.md",
            "repo/.claude/skills/perf-audit/SKILL.md",
        )
        sb.stage_fixture(
            "skills/.agents/skills/security-check/SKILL.md",
            "repo/.agents/skills/security-check/SKILL.md",
        )
        with DaemonDriver(sb) as dmn:
            ev_claude = dmn.request(
                {"op": "list_commands", "agent": "claude", "cwd": str(sb.home / "repo")},
                "commands",
            )
            claude_names = {str(i.get("name", "")) for i in ev_claude.get("items", [])}
            assert any("perf-audit" in n for n in claude_names), (
                f"Expected .claude/skills/perf-audit in agent='claude' ListCommands output, got {sorted(claude_names)}"
            )

            ev_agy = dmn.request(
                {"op": "list_commands", "agent": "antigravity", "cwd": str(sb.home / "repo")},
                "commands",
            )
            agy_names = {str(i.get("name", "")) for i in ev_agy.get("items", [])}
            assert any("security-check" in n for n in agy_names), (
                f"Expected .agents/skills/security-check in agent='antigravity' ListCommands output, got {sorted(agy_names)}"
            )

    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="slash", min_expected=1)
    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="chat_view", min_expected=3)


@e2e_test(
    id="T3-09",
    tier=3,
    area="pairwise",
    features=("F18", "F36"),
    title="Whole-conversation SearchHistory locates hit beyond 512 KiB tail -> paginated ReadHistory loads it",
    baseline=False,
)
def test_pairwise_search_beyond_tail_and_paginated_load() -> None:
    with IsolatedHome(prefix="mb_t3_09_") as sb:
        s_path = generate_search_claude_jsonl(
            sb.claude_projects_dir / "-tmp-t3srch" / "claude-deep.jsonl",
            occurrences=600,
            early_unique_token="ANCIENT_ARCHIVE_NEEDLE_0001",
        )
        assert s_path.stat().st_size > 524288
        with DaemonDriver(sb) as dmn:
            tail_page = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-deep",
                    "source_path": str(s_path),
                    "agent": "claude",
                    "max_bytes": 524288,
                },
                "history",
            )
            assert tail_page.get("has_more") is True, (
                f"Expected has_more=True on >512 KiB transcript, got {tail_page.get('has_more')!r}"
            )
            assert "ANCIENT_ARCHIVE_NEEDLE_0001" not in json.dumps(tail_page["messages"]), (
                "Expected early ancient token to fall outside 512 KiB tail page"
            )

            sr = dmn.request(
                {
                    "op": "search_history",
                    "request_id": "req-ancient",
                    "session_id": "claude-deep",
                    "source_path": str(s_path),
                    "agent": "claude",
                    "query": "ancient_archive_needle_0001",
                },
                "search_results",
            )
            assert len(sr["hits"]) == 1
            hit_offset = int(sr["hits"][0]["byte_offset"])
            assert hit_offset < int(tail_page["oldest_offset"])

            older = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-deep",
                    "source_path": str(s_path),
                    "agent": "claude",
                    "before_offset": tail_page["oldest_offset"],
                    "max_bytes": 524288,
                },
                "history",
            )
            assert "ANCIENT_ARCHIVE_NEEDLE_0001" in json.dumps(older["messages"])


@e2e_test(
    id="T3-10",
    tier=3,
    area="pairwise",
    features=("F27", "F23", "F34", "F36", "INV-4"),
    title="SharedAppStore coordinates multi-window sidebar, reactions & offline outbox with status-free chrome",
    baseline=False,
)
def test_pairwise_shared_store_sidebar_reactions_and_outbox() -> None:
    state_rs = REPO_ROOT / "crates" / "multibot-desktop" / "src" / "state.rs"
    assert state_rs.is_file() and "SharedAppStore" in state_rs.read_text(encoding="utf-8"), (
        "SharedAppStore missing in crates/multibot-desktop/src/state.rs"
    )
    assert not scan_chat_chrome_violations(REPO_ROOT), "Chat chrome invariant violated"
    CARGO.assert_crate_tests_pass("multibot-core", filter_expr="outbox", min_expected=2)
    CARGO.assert_crate_tests_pass("multibot-desktop", min_expected=8)
