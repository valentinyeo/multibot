"""Tier 4 Real-World Daily-Driver End-to-End Workflows (T4-01..07)."""

from __future__ import annotations

import json
import os
import re
from pathlib import Path

from e2e.lib import REPO_ROOT
from e2e.lib.cargo_runner import CARGO
from e2e.lib.daemon_driver import DaemonDriver, probe_cli_version
from e2e.lib.fixture_gen import (
    antigravity_history_filename,
    generate_large_claude_jsonl,
    generate_search_claude_jsonl,
    install_fake_agent_shims,
    wait_for_log_substring,
)
from e2e.lib.runner import e2e_test
from e2e.lib.sandbox import IsolatedHome
from e2e.lib.static_checks import (
    SLICE34_REQUIRED_WIRED_CHORDS,
    check_conditional_footprint_notify,
    check_desktop_cargo_gpui_features,
    check_dxgi_patch_invariants,
    check_oled_theme_tokens,
    find_ctrl_alt_bindings,
    parse_main_keybindings,
    parse_multibot_keys_shortcuts,
    read_crate_production_sources,
    scan_chat_chrome_violations,
    scan_update_copy_violations,
)
from e2e.lib.yaml_util import load_yaml


@e2e_test(
    id="T4-01",
    tier=4,
    area="workflow",
    features=("F1", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "INV-1", "INV-4"),
    title="Workflow 1: Zero-touch in-app update, ETag 304 -> v0.1.99 bundle, atomic swap & remote multibotd rollout",
    baseline=False,
)
def test_workflow_zero_touch_in_app_update_and_daemon_rollout() -> None:
    # 1. Verify CI produces the real parallel release pipeline
    wf = load_yaml(REPO_ROOT / ".github" / "workflows" / "ci.yml")
    jobs = wf.get("jobs", {})
    assert "release" in jobs and "linux" in jobs and "windows" in jobs, (
        f"ci.yml must define linux, windows, and release jobs; found {sorted(jobs.keys())}"
    )
    assert "linux" not in (jobs["windows"].get("needs") or []), (
        "ci.yml windows job must run in parallel with linux (must not need linux)"
    )

    # 2. Verify updater production contract (ETag/304, MULTIBOT_UPDATE_URL, --update-ready, rejected.txt, --gpu)
    upd_prod = read_crate_production_sources(REPO_ROOT / "crates" / "multibot-updater" / "src")
    assert "MULTIBOT_UPDATE_URL" in upd_prod, (
        "multibot-updater production code missing MULTIBOT_UPDATE_URL override"
    )
    assert "if-none-match" in upd_prod.lower(), (
        "multibot-updater production code missing If-None-Match ETag header"
    )
    assert re.search(r"\b304\b|\bNOT_MODIFIED\b", upd_prod), (
        "multibot-updater production code must handle HTTP 304 / StatusCode::NOT_MODIFIED"
    )
    for tok in ("--update-ready", "rejected.txt"):
        assert tok in upd_prod, (
            f"multibot-updater production code missing required update/relaunch token: {tok}"
        )
    has_cli_arg_forwarding = (
        "--gpu" in upd_prod
        or (
            "env::args" in upd_prod
            and "preserved_cli_args" in upd_prod
            and "spawn_relaunch" in upd_prod
        )
    )
    assert has_cli_arg_forwarding, (
        "multibot-updater production code must preserve CLI flags (including --gpu) across relaunch "
        "via explicit --gpu handling or std::env::args() -> preserved_cli_args() -> spawn_relaunch()"
    )

    # 3. Verify compiled updater, remote daemon deploy, Update Card UI copy, and multibotd --version/HelloAck
    copy_violations = scan_update_copy_violations(REPO_ROOT)
    assert not copy_violations, (
        f"Update card violated zero-duration copy rule: {copy_violations}"
    )
    rc, ver_out, ver_err = probe_cli_version(timeout=2.0)
    assert rc == 0 and re.search(r"\d+\.\d+\.\d+", ver_out), (
        f"multibotd --version must exit 0 and print SemVer version; "
        f"got rc={rc}, stdout={ver_out!r}, stderr={ver_err!r}"
    )
    with IsolatedHome(prefix="mb_t4_01_") as sb:
        with DaemonDriver(sb) as dmn:
            ack = dmn.request({"op": "hello", "client_version": "0.1.99"}, "hello_ack")
            bhash = str(ack.get("binary_hash", ""))
            assert re.match(r"^[0-9a-fA-F]{12,}$", bhash), (
                f"HelloAck must include non-empty >=12-hex binary_hash, got {ack!r}"
            )

    check_dxgi_patch_invariants(REPO_ROOT)
    CARGO.assert_crate_tests_pass(
        "multibot-updater",
        min_expected=8,
        required_any_of=[
            ("etag", "304", "not_modified", "if_none_match", "poll", "check_update", "latest"),
            ("swap", "apply", "relaunch", "rollback", "stage"),
        ],
    )
    CARGO.assert_crate_tests_pass("multibot-core", filter_expr="daemon_deploy", min_expected=2)
    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="update", min_expected=1)


@e2e_test(
    id="T4-02",
    tier=4,
    area="workflow",
    features=("F11", "F12", "F13", "F14", "F15", "F17", "F21", "F24", "F26"),
    title="Workflow 2: Multi-agent morning triage (Claude + Codex + Antigravity live stream -> Inbox -> Rename)",
    baseline=False,
)
def test_workflow_multi_agent_morning_triage() -> None:
    with IsolatedHome(prefix="mb_t4_02_") as sb:
        c_path = sb.stage_fixture(
            "claude/semantic_busy_open_tool.jsonl",
            ".claude/projects/-tmp-triage/claude-triage.jsonl",
            mtime_offset_sec=-60.0,
        )
        sb.stage_fixture(
            "codex/rollout-2026-11-26-codex-rich-001.jsonl",
            ".codex/sessions/2026/11/26/rollout-2026-11-26-codex-rich-001.jsonl",
            mtime_offset_sec=-20.0,
        )
        agy_fname = antigravity_history_filename("agy-triage-001")
        sb.stage_fixture(
            "antigravity/hosted_history.jsonl",
            f".multibot/history/{agy_fname}",
            mtime_offset_sec=-10.0,
        )

        with DaemonDriver(sb) as dmn:
            sessions = dmn.request({"op": "list_sessions", "limit": 20}, "sessions")["sessions"]
            agents = {s["agent"] for s in sessions}
            assert agents == {"claude", "codex", "antigravity"}, (
                f"Morning triage must discover all 3 agents, got {agents}"
            )

            dmn.send({"op": "watch_directory"})
            dmn.request(
                {
                    "op": "watch_session",
                    "session_id": "claude-triage",
                    "source_path": str(c_path),
                    "agent": "claude",
                },
                "history",
            )

            # Finish Claude's open tool and assistant reply -> live HistoryAppend flips working to false
            with c_path.open("a", encoding="utf-8") as fh:
                fh.write(
                    json.dumps(
                        {
                            "type": "user",
                            "uuid": "u-triage-res",
                            "timestamp": "2026-11-26T12:15:00Z",
                            "message": {
                                "content": [
                                    {
                                        "type": "tool_result",
                                        "tool_use_id": "tu_open_1",
                                        "is_error": False,
                                        "content": "14 passed",
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
                            "uuid": "a-triage-done",
                            "timestamp": "2026-11-26T12:15:01Z",
                            "message": {
                                "content": [{"type": "text", "text": "Morning triage report ready."}]
                            },
                        }
                    )
                    + "\n"
                )
                fh.flush()
                os.fsync(fh.fileno())

            app = dmn.wait_for(
                "history_append",
                predicate=lambda e: e.get("session_id") == "claude-triage"
                and "Morning triage report ready." in json.dumps(e),
                timeout=2.0,
            )
            assert app.get("working") is False, (
                f"Expected live history_append to flip working=False after completion, got {app!r}"
            )

            # Rename session via F2 / RenameSession wire op
            upd = dmn.request(
                {
                    "op": "rename_session",
                    "session_id": "claude-triage",
                    "agent": "claude",
                    "source_path": str(c_path),
                    "title": "Morning Release Triage",
                },
                "session_updated",
                predicate=lambda e: e.get("session", {}).get("id") == "claude-triage"
                and e.get("session", {}).get("title") == "Morning Release Triage",
            )
            assert upd["session"]["title"] == "Morning Release Triage", (
                f"Expected renamed session title 'Morning Release Triage', got {upd!r}"
            )

    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="inbox", min_expected=2)
    CARGO.assert_crate_tests_pass("multibot-core", filter_expr="workspaces", min_expected=3)


@e2e_test(
    id="T4-03",
    tier=4,
    area="workflow",
    features=("F2", "F11", "F12", "F18", "F29", "F30", "F31", "F32", "F36", "F37"),
    title="Workflow 3: Deep coding session (rich diffs/todos/options, pagination, search & <16ms two-tier cache)",
    baseline=False,
)
def test_workflow_deep_coding_session_and_fast_cache() -> None:
    feats = check_desktop_cargo_gpui_features(REPO_ROOT)
    assert "syntect" in feats, (
        f"multibot-desktop gpui-kit features must include 'syntect', got {sorted(feats)}"
    )

    with IsolatedHome(prefix="mb_t4_03_") as sb:
        rich_path = sb.stage_fixture(
            "claude/rich_slice2.jsonl",
            ".claude/projects/-tmp-deep/claude-deep-rich.jsonl",
        )
        large_path, _ = generate_large_claude_jsonl(
            sb.claude_projects_dir / "-tmp-deep" / "claude-deep-large.jsonl",
            min_bytes=660 * 1024,
        )
        with DaemonDriver(sb) as dmn:
            h_rich = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-deep-rich",
                    "source_path": str(rich_path),
                    "agent": "claude",
                },
                "history",
            )
            assert any(m.get("blocks") for m in h_rich["messages"]), (
                "Expected rich structured MessageBlock items in Claude ReadHistory"
            )
            assert h_rich.get("todos") or any(m.get("todos") for m in h_rich["messages"]), (
                "Expected TodoWrite todos in Claude ReadHistory"
            )

            p1 = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-deep-large",
                    "source_path": str(large_path),
                    "agent": "claude",
                    "max_bytes": 524288,
                },
                "history",
            )
            assert p1.get("has_more") is True and p1.get("probe_sha256"), (
                f"Expected paginated ReadHistory to set has_more=True and non-empty probe_sha256, "
                f"got has_more={p1.get('has_more')!r}, probe_sha256={p1.get('probe_sha256')!r}"
            )

            sr = dmn.request(
                {
                    "op": "search_history",
                    "request_id": "req-deep",
                    "session_id": "claude-deep-large",
                    "source_path": str(large_path),
                    "agent": "claude",
                    "query": "TURN_MARKER_0001",
                },
                "search_results",
            )
            assert len(sr["hits"]) >= 1, (
                f"Expected >=1 search_history hit for TURN_MARKER_0001, got {sr!r}"
            )

    CARGO.assert_crate_tests_pass("multibot-core", filter_expr="cache", min_expected=3)
    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="chat_view", min_expected=3)
    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="table", min_expected=1)


@e2e_test(
    id="T4-04",
    tier=4,
    area="workflow",
    features=("F13", "F16", "F17", "F19", "F33", "F34", "F36", "INV-4"),
    title="Workflow 4: Live steering, interrupt, slash skills, Quote/Reply /skill hoist, reactions & offline outbox",
    baseline=False,
)
def test_workflow_live_steering_skills_quote_reactions_and_outbox() -> None:
    chrome_violations = scan_chat_chrome_violations(REPO_ROOT)
    assert not chrome_violations, (
        f"Forbidden legacy chat chrome strings detected: {chrome_violations}"
    )

    with IsolatedHome(prefix="mb_t4_04_") as sb:
        log_path = install_fake_agent_shims(sb)
        sb.stage_fixture(
            "skills/.claude/commands/deploy-vps.md",
            "repo/.claude/commands/deploy-vps.md",
        )
        sb.stage_fixture(
            "skills/.agents/skills/security-check/SKILL.md",
            "repo/.agents/skills/security-check/SKILL.md",
        )
        proj = sb.home / "repo"

        with DaemonDriver(
            sb, use_fake_bin=True, extra_env={"FAKE_AGENT_SLOW": "1"}
        ) as dmn:
            cmds = dmn.request(
                {"op": "list_commands", "agent": "claude", "cwd": str(proj)},
                "commands",
            )
            names = {str(i.get("name", "")) for i in cmds.get("items", [])}
            assert any("deploy-vps" in n for n in names), (
                f"Expected deploy-vps command from ListCommands, got {sorted(names)}"
            )

            spawned = dmn.request(
                {
                    "op": "spawn_session",
                    "agent": "antigravity",
                    "cwd": str(proj),
                    "title": "Hosted Steer Workflow",
                    "initial_prompt": "Begin slow audit",
                },
                "session_spawned",
                timeout=4.0,
            )
            sid = spawned["session"]["id"]
            boot_log = wait_for_log_substring(log_path, "agy:stdin:", timeout=3.0)
            assert "agy:stdin:" in boot_log, (
                f"Expected fake agy shim to start and receive initial prompt before interrupt; log={boot_log!r}"
            )

            dmn.request(
                {
                    "op": "steer_session",
                    "session_id": sid,
                    "agent": "antigravity",
                    "cwd": str(proj),
                    "text": "/security-check focus on updater",
                },
                "prompt_status",
                timeout=3.0,
            )

            inter = dmn.request(
                {"op": "interrupt_session", "session_id": sid, "agent": "antigravity"},
                "session_interrupted",
                timeout=3.0,
            )
            assert inter["ok"] is True, (
                f"Expected interrupt_session to succeed (ok=True), got {inter!r}"
            )
            log_text = wait_for_log_substring(log_path, "SIGINT", timeout=1.5)
            assert "SIGINT" in log_text, (
                f"Expected fake agy shim to record SIGINT from interrupt_session; log={log_text!r}"
            )

    CARGO.assert_crate_tests_pass("multibot-core", filter_expr="outbox", min_expected=2)
    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="slash", min_expected=1)
    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="chat_view", min_expected=3)


@e2e_test(
    id="T4-05",
    tier=4,
    area="workflow",
    features=("F20", "F21", "F22", "F23", "F25", "F27", "INV-1", "INV-2"),
    title="Workflow 5: Legacy Multiprompt read-only migration, 0% NVIDIA audit, Alt+1..9 workspaces & Ctrl+K frecency",
    baseline=False,
)
def test_workflow_legacy_migration_gpu_audit_and_palette() -> None:
    check_dxgi_patch_invariants(REPO_ROOT)

    with IsolatedHome(prefix="mb_t4_05_") as sb:
        snap = sb.stage_zigshel_fixtures(readonly=True)
        CARGO.assert_crate_tests_pass("multibot-gpu", min_expected=5)
        CARGO.assert_crate_tests_pass(
            "multibot-core", filter_expr="zigshel_import", min_expected=2
        )
        CARGO.assert_crate_tests_pass(
            "multibot-core", filter_expr="workspaces", min_expected=3
        )
        CARGO.assert_crate_tests_pass(
            "multibot-desktop", filter_expr="palette", min_expected=2
        )
        CARGO.assert_crate_tests_pass(
            "multibot-desktop", filter_expr="sidebar", min_expected=2
        )
        sb.assert_unchanged(sb.zigshel_dir, snap)


@e2e_test(
    id="T4-06",
    tier=4,
    area="workflow",
    features=("F6", "F7", "F8", "F14", "F17", "F37"),
    title="Workflow 6: Crash, tamper, zip-slip, poison-pill rollback, partial-line/truncation & archive resilience",
    baseline=False,
)
def test_workflow_crash_tamper_poison_pill_and_stream_resilience() -> None:
    # 1. Verify updater tamper-rejection, rollback & poison-pill behavioral coverage
    CARGO.assert_crate_tests_pass(
        "multibot-updater",
        min_expected=8,
        required_any_of=[
            (
                "tamper",
                "mismatch",
                "corrupt",
                "bitflip",
                "bad_sha",
                "wrong_sha",
                "invalid_sha",
                "bad_hash",
                "wrong_hash",
                "checksum_fail",
                "digest_mismatch",
                "hash_mismatch",
                "sha256_mismatch",
                "verify_fail",
                "rejects_bad",
            ),
            ("rollback", "restore", "rejected", "poison", "update_ready", "readiness"),
        ],
    )
    CARGO.assert_crate_tests_pass("multibot-core", filter_expr="cache", min_expected=4)

    # 2. Verify live daemon survives partial line, truncation, and archive/unarchive without transcript loss
    with IsolatedHome(prefix="mb_t4_06_") as sb:
        c_path = sb.stage_fixture(
            "claude/basic_slice1.jsonl",
            ".claude/projects/-tmp-resil/claude-resil.jsonl",
        )
        orig_bytes = c_path.read_bytes()
        with DaemonDriver(sb) as dmn:
            dmn.request(
                {
                    "op": "watch_session",
                    "session_id": "claude-resil",
                    "source_path": str(c_path),
                    "agent": "claude",
                },
                "history",
            )
            with c_path.open("a", encoding="utf-8") as fh:
                fh.write('{"type":"assistant","uuid":"a-p","timestamp":"2026-11-26T12:30:00Z","message":{"content":[{"type":"text","text":"RESIL_')
                fh.flush()
            dmn.expect_no_event("history_append", duration=0.20)
            with c_path.open("a", encoding="utf-8") as fh:
                fh.write('OK"}]}}\n')
                fh.flush()
            ev = dmn.wait_for("history_append", timeout=2.0)
            assert "RESIL_OK" in json.dumps(ev), (
                f"Expected split-line RESIL_OK payload in history_append, got {ev!r}"
            )

            # Archive then unarchive; transcript must stay intact on disk
            a1 = dmn.request(
                {
                    "op": "archive_session",
                    "session_id": "claude-resil",
                    "agent": "claude",
                    "source_path": str(c_path),
                    "kill_process": False,
                },
                "session_archived",
            )
            assert a1["archived"] is True and c_path.is_file(), (
                f"Expected archive_session to mark archived=True while keeping transcript on disk; "
                f"event={a1!r}, exists={c_path.is_file()}"
            )
            assert c_path.read_bytes().startswith(orig_bytes), (
                "Archiving session modified or truncated original transcript bytes on disk"
            )

            a2 = dmn.request(
                {"op": "unarchive_session", "session_id": "claude-resil"},
                "session_archived",
            )
            assert a2["archived"] is False, (
                f"Expected unarchive_session to restore archived=False, got {a2!r}"
            )


@e2e_test(
    id="T4-07",
    tier=4,
    area="workflow",
    features=("F20", "F27", "F28", "F35", "F38", "INV-3", "INV-4"),
    title="Workflow 7: Multi-window SharedAppStore, attachments, complete shortcut & AltGr audit, OLED & demand-driven check",
    baseline=False,
)
def test_workflow_multi_window_attachments_and_full_invariant_audit() -> None:
    state_rs = REPO_ROOT / "crates" / "multibot-desktop" / "src" / "state.rs"
    assert state_rs.is_file() and "SharedAppStore" in state_rs.read_text(encoding="utf-8"), (
        f"Expected {state_rs} to exist and define SharedAppStore GPUI Global"
    )

    check_oled_theme_tokens(REPO_ROOT)
    check_conditional_footprint_notify(REPO_ROOT)
    ctrl_alt_violations = find_ctrl_alt_bindings(REPO_ROOT)
    assert not ctrl_alt_violations, (
        f"AltGr-unsafe Ctrl+Alt keybindings detected: {ctrl_alt_violations}"
    )
    chrome_violations = scan_chat_chrome_violations(REPO_ROOT)
    assert not chrome_violations, (
        f"Forbidden legacy chat chrome strings detected: {chrome_violations}"
    )

    entries = parse_multibot_keys_shortcuts(REPO_ROOT)
    wired = {str(e["gpui_key"]) for e in entries if e["wired"]}
    assert SLICE34_REQUIRED_WIRED_CHORDS.issubset(wired), (
        f"Missing wired Slice 3/4 shortcuts: {sorted(SLICE34_REQUIRED_WIRED_CHORDS - wired)}"
    )
    bound = set(parse_main_keybindings(REPO_ROOT))
    assert SLICE34_REQUIRED_WIRED_CHORDS.issubset(bound), (
        f"Missing KeyBinding::new for Slice 3/4 shortcuts: {sorted(SLICE34_REQUIRED_WIRED_CHORDS - bound)}"
    )

    CARGO.assert_crate_tests_pass("multibot-keys", min_expected=2)
    CARGO.assert_crate_tests_pass("multibot-desktop", min_expected=8)
