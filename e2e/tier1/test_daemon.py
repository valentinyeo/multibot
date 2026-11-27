"""Tier 1 multibotd --stdio tests: Slice 1 baseline (T1-DMN-01..05) + Slice 2 rich daemon (T1-DMN-06..12)."""

from __future__ import annotations

import json
import os
import time
from pathlib import Path
from typing import Any, Dict, List

from e2e.lib.daemon_driver import DaemonDriver
from e2e.lib.fixture_gen import (
    antigravity_history_filename,
    generate_large_claude_jsonl,
    generate_search_claude_jsonl,
    install_fake_agent_shims,
    wait_for_log_substring,
)
from e2e.lib.runner import e2e_test
from e2e.lib.sandbox import IsolatedHome


def _all_blocks(messages: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
    blocks: List[Dict[str, Any]] = []
    for msg in messages:
        for b in msg.get("blocks") or []:
            if isinstance(b, dict):
                blocks.append(b)
    return blocks


def _block_kind(block: Dict[str, Any]) -> str:
    """Support either tagged (`kind`/`type`) or externally tagged serde representations."""
    for tag in ("kind", "type"):
        if tag in block:
            return str(block[tag]).lower()
    if len(block) == 1:
        return next(iter(block.keys())).lower()
    return ""


def _block_body(block: Dict[str, Any]) -> Dict[str, Any]:
    if "kind" in block or "type" in block:
        return block
    if len(block) == 1:
        val = next(iter(block.values()))
        if isinstance(val, dict):
            return val
    return block


# ---------------------------------------------------------------------------
# Slice 1 Baseline Tests (T1-DMN-01..05) — MUST PASS IMMEDIATELY
# ---------------------------------------------------------------------------


@e2e_test(
    id="T1-DMN-01",
    tier=1,
    area="daemon",
    features=("F1",),
    title="Slice 1 hello/hello_ack & ping/pong NDJSON roundtrip",
    baseline=True,
)
def test_slice1_hello_and_ping_roundtrip() -> None:
    with IsolatedHome(prefix="mb_t1_dmn01_") as sb:
        with DaemonDriver(sb) as dmn:
            ack = dmn.request({"op": "hello", "client_version": "0.1.0"}, "hello_ack")
            assert ack["protocol_version"] == 1
            assert isinstance(ack.get("daemon_version"), str) and ack["daemon_version"]
            assert isinstance(ack.get("hostname"), str) and ack["hostname"]
            assert isinstance(ack.get("pid"), int) and ack["pid"] > 0

            pong = dmn.request({"op": "ping", "nonce": 9001}, "pong")
            assert pong["nonce"] == 9001
            assert isinstance(pong.get("server_time_ms"), int) and pong["server_time_ms"] > 0


@e2e_test(
    id="T1-DMN-02",
    tier=1,
    area="daemon",
    features=("F11", "F12", "F13"),
    title="Slice 1 list_sessions discovers Claude, Codex & Antigravity in isolated HOME",
    baseline=True,
)
def test_slice1_list_sessions_all_providers_isolated() -> None:
    with IsolatedHome(prefix="mb_t1_dmn02_") as sb:
        sb.stage_fixture(
            "claude/basic_slice1.jsonl",
            ".claude/projects/-tmp-proj-basic/claude-basic-001.jsonl",
            mtime_offset_sec=-30,
        )
        sb.stage_fixture(
            "codex/basic_slice1.jsonl",
            ".codex/sessions/2026/11/26/rollout-2026-11-26-codex-basic-001.jsonl",
            mtime_offset_sec=-15,
        )
        sb.stage_fixture(
            "antigravity/brain_session.jsonl",
            ".gemini/antigravity/brain/agy-brain-001/transcript.jsonl",
            mtime_offset_sec=-5,
        )
        # The desktop lists only chats something still holds open, so each fixture gets the
        # legacy host record that marks it live.
        sb.stage_live_legacy_chat(tmux="live-claude-basic", sid="claude-basic-001")
        sb.stage_live_legacy_chat(
            tmux="live-codex-basic",
            sid="codex-basic-001",
            provider="codex",
        )
        sb.stage_live_legacy_chat(
            tmux="live-agy-brain",
            conversation="agy-brain-001",
            provider="antigravity",
        )

        with DaemonDriver(sb) as dmn:
            ev = dmn.request({"op": "list_sessions", "limit": 20}, "sessions")
            sessions = ev["sessions"]
            assert len(sessions) == 3, f"Expected 3 isolated sessions, got {len(sessions)}: {sessions!r}"
            agents = {s["agent"] for s in sessions}
            assert agents == {"claude", "codex", "antigravity"}, f"Unexpected agents: {agents}"
            for s in sessions:
                assert str(sb.home) in s["source_path"], (
                    f"Session leaked outside isolated HOME: {s['source_path']}"
                )
            mtimes = [s["updated_at_ms"] for s in sessions]
            assert mtimes == sorted(mtimes, reverse=True), (
                f"Sessions must be sorted newest-first by updated_at_ms: {mtimes}"
            )


@e2e_test(
    id="T1-DMN-03",
    tier=1,
    area="daemon",
    features=("F11", "F12", "F13"),
    title="Slice 1 read_history across Claude, Codex & Antigravity + tail limit",
    baseline=True,
)
def test_slice1_read_history_and_tail() -> None:
    with IsolatedHome(prefix="mb_t1_dmn03_") as sb:
        c_path = sb.stage_fixture(
            "claude/basic_slice1.jsonl",
            ".claude/projects/-tmp-proj-basic/claude-basic-001.jsonl",
        )
        x_path = sb.stage_fixture(
            "codex/basic_slice1.jsonl",
            ".codex/sessions/2026/11/26/rollout-2026-11-26-codex-basic-001.jsonl",
        )
        a_path = sb.stage_fixture(
            "antigravity/brain_session.jsonl",
            ".gemini/antigravity/brain/agy-brain-001/transcript.jsonl",
        )

        with DaemonDriver(sb) as dmn:
            h_claude = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-basic-001",
                    "source_path": str(c_path),
                    "agent": "claude",
                    "tail": 50,
                },
                "history",
            )
            assert len(h_claude["messages"]) == 4
            assert [m["role"] for m in h_claude["messages"]] == [
                "user",
                "assistant",
                "user",
                "assistant",
            ]

            h_tail = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-basic-001",
                    "source_path": str(c_path),
                    "agent": "claude",
                    "tail": 2,
                },
                "history",
            )
            assert len(h_tail["messages"]) == 2
            assert "0x10DE" in h_tail["messages"][-1]["text"]

            h_codex = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "codex-basic-001",
                    "source_path": str(x_path),
                    "agent": "codex",
                    "tail": 50,
                },
                "history",
            )
            assert len(h_codex["messages"]) >= 2

            h_agy = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "agy-brain-001",
                    "source_path": str(a_path),
                    "agent": "antigravity",
                    "tail": 50,
                },
                "history",
            )
            assert len(h_agy["messages"]) == 2


@e2e_test(
    id="T1-DMN-04",
    tier=1,
    area="daemon",
    features=("F14",),
    title="Slice 1 watch_session emits initial history & live inotify history_append",
    baseline=True,
)
def test_slice1_watch_session_live_inotify_append() -> None:
    with IsolatedHome(prefix="mb_t1_dmn04_") as sb:
        c_path = sb.stage_fixture(
            "claude/basic_slice1.jsonl",
            ".claude/projects/-tmp-proj-basic/claude-basic-001.jsonl",
        )
        with DaemonDriver(sb) as dmn:
            init_h = dmn.request(
                {
                    "op": "watch_session",
                    "session_id": "claude-basic-001",
                    "source_path": str(c_path),
                    "agent": "claude",
                },
                "history",
            )
            assert len(init_h["messages"]) == 4

            new_line = json.dumps(
                {
                    "type": "assistant",
                    "uuid": "a-live-99",
                    "timestamp": "2026-11-26T10:02:00Z",
                    "message": {
                        "content": [{"type": "text", "text": "LIVE_INOTIFY_DELTA_OK"}]
                    },
                },
                separators=(",", ":"),
            )
            with c_path.open("a", encoding="utf-8") as fh:
                fh.write(new_line + "\n")
                fh.flush()
                os.fsync(fh.fileno())

            app = dmn.wait_for(
                "history_append",
                predicate=lambda ev: ev.get("session_id") == "claude-basic-001",
                timeout=2.0,
            )
            joined = " ".join(m.get("text", "") for m in app.get("messages", []))
            assert "LIVE_INOTIFY_DELTA_OK" in joined, f"Missing live appended text in {app!r}"


@e2e_test(
    id="T1-DMN-05",
    tier=1,
    area="daemon",
    features=("F14",),
    title="Slice 1 unwatch_session stops live history_append while daemon stays responsive",
    baseline=True,
)
def test_slice1_unwatch_stops_events() -> None:
    with IsolatedHome(prefix="mb_t1_dmn05_") as sb:
        c_path = sb.stage_fixture(
            "claude/basic_slice1.jsonl",
            ".claude/projects/-tmp-proj-basic/claude-basic-001.jsonl",
        )
        with DaemonDriver(sb) as dmn:
            dmn.request(
                {
                    "op": "watch_session",
                    "session_id": "claude-basic-001",
                    "source_path": str(c_path),
                    "agent": "claude",
                },
                "history",
            )
            dmn.send({"op": "unwatch_session"})
            pong = dmn.request({"op": "ping", "nonce": 77}, "pong")
            assert pong["nonce"] == 77

            extra = json.dumps(
                {
                    "type": "assistant",
                    "uuid": "a-after-unwatch",
                    "timestamp": "2026-11-26T10:03:00Z",
                    "message": {"content": [{"type": "text", "text": "AFTER_UNWATCH"}]},
                }
            )
            with c_path.open("a", encoding="utf-8") as fh:
                fh.write(extra + "\n")
                fh.flush()
            dmn.expect_no_event("history_append", duration=0.35)


# ---------------------------------------------------------------------------
# Slice 2 Rich Daemon Tests (T1-DMN-06..12) — Real Protocol; Fail Until M3
# ---------------------------------------------------------------------------


@e2e_test(
    id="T1-DMN-06",
    tier=1,
    area="daemon",
    features=("F11",),
    title="Claude rich parsing: tag cleanup, paired tools, 40-line diff, TodoWrite & AskUserQuestion",
    baseline=False,
)
def test_slice2_claude_rich_parsing() -> None:
    with IsolatedHome(prefix="mb_t1_dmn06_") as sb:
        c_path = sb.stage_fixture(
            "claude/rich_slice2.jsonl",
            ".claude/projects/-tmp-proj-rich/claude-rich-001.jsonl",
        )
        with DaemonDriver(sb) as dmn:
            hist = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-rich-001",
                    "source_path": str(c_path),
                    "agent": "claude",
                },
                "history",
            )
            msgs = hist["messages"]
            assert msgs, "Expected parsed Claude messages"

            # (a) User tag cleanup (separate command-name and pasted_content turns)
            user_msgs = [m for m in msgs if m["role"] == "user"]
            assert len(user_msgs) >= 2, (
                f"Expected at least 2 cleaned user messages (command + pasted_content), got {user_msgs!r}"
            )
            user_texts = [str(m.get("text", "")) for m in user_msgs]
            assert any("/review src/engine.rs" in t for t in user_texts), (
                f"Expected cleaned '/review src/engine.rs' command in user texts, got {user_texts!r}"
            )
            assert any("hello pasted block" in t for t in user_texts), (
                f"Expected cleaned 'hello pasted block' pasted_content in user texts, got {user_texts!r}"
            )
            combined_utext = "\n".join(user_texts)
            for forbidden in (
                "<system-reminder>",
                "<local-command-stdout>",
                "[mp:0123456789abcdef]",
                "[mp:fedcba9876543210]",
            ):
                assert forbidden not in combined_utext, (
                    f"Uncleaned Claude tag {forbidden!r} in {user_texts!r}"
                )

            # (b) Paired ToolUse blocks (ok + error) and DiffBlock (55 lines -> <=40 + truncated_lines>=15)
            blocks = _all_blocks(msgs)
            tool_blocks = [_block_body(b) for b in blocks if "tool" in _block_kind(b)]
            assert tool_blocks, f"Expected structured MessageBlock::ToolUse blocks, got msgs={msgs!r}"
            statuses = {str(t.get("status", "")).lower() for t in tool_blocks}
            assert "error" in statuses, f"Expected ToolStatus::Error for failing Bash tool, got {statuses}"

            diffs = [t["diff"] for t in tool_blocks if isinstance(t.get("diff"), dict)]
            big_diff = next((d for d in diffs if d.get("added") == 30 and d.get("removed") == 15), None)
            assert big_diff is not None, f"Expected structuredPatch diff (+30 -15), got diffs={diffs!r}"
            assert len(big_diff.get("lines", [])) <= 40, (
                f"Diff lines must be capped at 40, got {len(big_diff.get('lines', []))}"
            )
            assert big_diff.get("truncated_lines", 0) >= 15, (
                f"Expected truncated_lines >= 15 (55 raw lines or 56 with @@ hunk header minus 40), got {big_diff!r}"
            )

            # (c) TodoWrite todos
            todos = hist.get("todos") or []
            if not todos:
                for m in msgs:
                    if m.get("todos"):
                        todos = m["todos"]
            assert len(todos) == 3, f"Expected 3 TodoWrite items, got {todos!r}"

            # (d) AskUserQuestion QuestionPrompt
            q_blocks = [_block_body(b) for b in blocks if "question" in _block_kind(b)]
            assert q_blocks, f"Expected QuestionPrompt block for AskUserQuestion, got blocks={blocks!r}"
            assert any(q.get("prompt_id") == "tu_ask_1" for q in q_blocks), (
                f"Expected prompt_id='tu_ask_1', got {q_blocks!r}"
            )


@e2e_test(
    id="T1-DMN-07",
    tier=1,
    area="daemon",
    features=("F12",),
    title="Codex rich parsing: apply_patch diffs, local_shell_call error, update_plan, request_user_input & model",
    baseline=False,
)
def test_slice2_codex_rich_parsing() -> None:
    with IsolatedHome(prefix="mb_t1_dmn07_") as sb:
        x_path = sb.stage_fixture(
            "codex/rollout-2026-11-26-codex-rich-001.jsonl",
            ".codex/sessions/2026/11/26/rollout-2026-11-26-codex-rich-001.jsonl",
        )
        sb.stage_live_legacy_chat(tmux="live-codex-rich", sid="codex-rich-001", provider="codex")
        with DaemonDriver(sb) as dmn:
            sess_ev = dmn.request({"op": "list_sessions", "limit": 20}, "sessions")
            codex_sess = next((s for s in sess_ev["sessions"] if s["agent"] == "codex"), None)
            assert codex_sess is not None, "Codex session not discovered"
            assert codex_sess.get("model") == "gpt-5.4", (
                f"Expected Codex turn_context model='gpt-5.4', got {codex_sess!r}"
            )

            hist = dmn.request(
                {
                    "op": "read_history",
                    "session_id": codex_sess["id"],
                    "source_path": str(x_path),
                    "agent": "codex",
                },
                "history",
            )
            blocks = _all_blocks(hist["messages"])
            tool_blocks = [_block_body(b) for b in blocks if "tool" in _block_kind(b)]
            assert tool_blocks, f"Expected Codex ToolUse blocks, got {hist['messages']!r}"

            diffs = [t["diff"] for t in tool_blocks if isinstance(t.get("diff"), dict)]
            assert any(d.get("added") == 2 and d.get("removed") == 1 for d in diffs), (
                f"Expected apply_patch diff (+2 -1), got {diffs!r}"
            )
            assert any(str(t.get("status", "")).lower() == "error" for t in tool_blocks), (
                f"Expected local_shell_call exit_code=1 to map to ToolStatus::Error: {tool_blocks!r}"
            )

            todos = hist.get("todos") or []
            if not todos:
                for m in hist["messages"]:
                    if m.get("todos"):
                        todos = m["todos"]
            assert len(todos) == 3, f"Expected 3 update_plan todo items, got {todos!r}"

            q_blocks = [_block_body(b) for b in blocks if "question" in _block_kind(b)]
            assert q_blocks, f"Expected QuestionPrompt from request_user_input, got {blocks!r}"


@e2e_test(
    id="T1-DMN-08",
    tier=1,
    area="daemon",
    features=("F13",),
    title="Antigravity daemon-owned ~/.multibot/history/<sha32>.history.jsonl discovery & read",
    baseline=False,
)
def test_slice2_antigravity_daemon_owned_history() -> None:
    with IsolatedHome(prefix="mb_t1_dmn08_") as sb:
        fname = antigravity_history_filename("agy-hosted-001")
        h_path = sb.stage_fixture(
            "antigravity/hosted_history.jsonl",
            f".multibot/history/{fname}",
        )
        sb.stage_fixture(
            "antigravity/brain_session.jsonl",
            ".gemini/antigravity/brain/agy-brain-001/transcript.jsonl",
        )
        sb.stage_live_legacy_chat(
            tmux="live-agy-hosted", sid="agy-hosted-001", provider="antigravity"
        )
        sb.stage_live_legacy_chat(
            tmux="live-agy-brain", conversation="agy-brain-001", provider="antigravity"
        )
        with DaemonDriver(sb) as dmn:
            sess_ev = dmn.request({"op": "list_sessions", "limit": 20}, "sessions")
            agy_sessions = [s for s in sess_ev["sessions"] if s["agent"] == "antigravity"]
            hosted = next((s for s in agy_sessions if fname in s["source_path"]), None)
            assert hosted is not None, (
                f"Daemon-owned Antigravity history {fname} not discovered in {agy_sessions!r}"
            )
            hist = dmn.request(
                {
                    "op": "read_history",
                    "session_id": hosted["id"],
                    "source_path": str(h_path),
                    "agent": "antigravity",
                },
                "history",
            )
            roles = [m["role"] for m in hist["messages"]]
            assert "user" in roles and "assistant" in roles, (
                f"Expected parsed user+assistant rows from multiprompt_history, got {hist['messages']!r}"
            )


@e2e_test(
    id="T1-DMN-09",
    tier=1,
    area="daemon",
    features=("F14", "F15"),
    title="Multi-session concurrent inotify watches & watch_directory SessionUpdated",
    baseline=False,
)
def test_slice2_multi_session_and_directory_watch() -> None:
    with IsolatedHome(prefix="mb_t1_dmn09_") as sb:
        c_path = sb.stage_fixture(
            "claude/basic_slice1.jsonl",
            ".claude/projects/-tmp-proj-a/claude-a.jsonl",
        )
        x_path = sb.stage_fixture(
            "codex/basic_slice1.jsonl",
            ".codex/sessions/2026/11/26/rollout-2026-11-26-codex-b.jsonl",
        )
        with DaemonDriver(sb) as dmn:
            dmn.send({"op": "watch_directory"})
            dmn.request(
                {
                    "op": "watch_session",
                    "session_id": "claude-a",
                    "source_path": str(c_path),
                    "agent": "claude",
                },
                "history",
            )
            dmn.request(
                {
                    "op": "watch_session",
                    "session_id": "codex-b",
                    "source_path": str(x_path),
                    "agent": "codex",
                },
                "history",
            )

            # Append to Session A (must STILL be watched after watching Session B!)
            with c_path.open("a", encoding="utf-8") as fh:
                fh.write(
                    json.dumps(
                        {
                            "type": "assistant",
                            "uuid": "a-multi-1",
                            "timestamp": "2026-11-26T10:10:00Z",
                            "message": {"content": [{"type": "text", "text": "MULTI_WATCH_A_OK"}]},
                        }
                    )
                    + "\n"
                )
                fh.flush()
            ev_a = dmn.wait_for(
                "history_append",
                predicate=lambda ev: ev.get("session_id") == "claude-a",
                timeout=2.0,
            )
            assert "MULTI_WATCH_A_OK" in json.dumps(ev_a)
            assert ev_a.get("latest_offset") is not None and ev_a.get("probe_sha256"), (
                f"Slice 2 history_append must include latest_offset & probe_sha256: {ev_a!r}"
            )

            # Append to Session B
            with x_path.open("a", encoding="utf-8") as fh:
                fh.write(
                    json.dumps(
                        {
                            "type": "response_item",
                            "timestamp": "2026-11-26T10:10:01Z",
                            "payload": {
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": "MULTI_WATCH_B_OK"}],
                            },
                        }
                    )
                    + "\n"
                )
                fh.flush()
            ev_b = dmn.wait_for(
                "history_append",
                predicate=lambda ev: ev.get("session_id") == "codex-b",
                timeout=2.0,
            )
            assert "MULTI_WATCH_B_OK" in json.dumps(ev_b)

            # Create a brand-new session file -> expect directory watcher SessionUpdated
            new_file = sb.home / ".claude" / "projects" / "-tmp-proj-a" / "claude-brand-new.jsonl"
            new_file.write_text(
                json.dumps(
                    {
                        "type": "user",
                        "uuid": "u-new-1",
                        "cwd": "/tmp/proj/a",
                        "timestamp": "2026-11-26T10:10:02Z",
                        "message": {"content": "Brand new session"},
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            upd = dmn.wait_for("session_updated", timeout=2.0)
            assert "session" in upd


@e2e_test(
    id="T1-DMN-10",
    tier=1,
    area="daemon",
    features=("F17",),
    title="Session lifecycle: spawn_session, rename_session, archive_session & unarchive_session",
    baseline=False,
)
def test_slice2_spawn_rename_archive_unarchive() -> None:
    with IsolatedHome(prefix="mb_t1_dmn10_") as sb:
        install_fake_agent_shims(sb)
        proj = sb.home / "repo"
        proj.mkdir(parents=True, exist_ok=True)
        with DaemonDriver(sb, use_fake_bin=True) as dmn:
            spawned = dmn.request(
                {
                    "op": "spawn_session",
                    "agent": "antigravity",
                    "cwd": str(proj),
                    "title": "Initial Spawn Title",
                    "initial_prompt": "Hello hosted agy",
                },
                "session_spawned",
                timeout=4.0,
            )
            sess = spawned["session"]
            sid = sess["id"]
            src_path = Path(sess["source_path"])

            dmn.request(
                {
                    "op": "rename_session",
                    "session_id": sid,
                    "agent": "antigravity",
                    "source_path": str(src_path),
                    "title": "Renamed Production Bot",
                },
                "session_updated",
                timeout=3.0,
            )

            arch = dmn.request(
                {
                    "op": "archive_session",
                    "session_id": sid,
                    "agent": "antigravity",
                    "source_path": str(src_path),
                    "kill_process": True,
                },
                "session_archived",
                timeout=3.0,
            )
            assert arch["session_id"] == sid and arch["archived"] is True
            assert (sb.multibot_dir / "archive.json").is_file(), (
                "archive_session must persist ~/.multibot/archive.json"
            )
            if src_path.exists():
                assert src_path.stat().st_size >= 0

            unarch = dmn.request(
                {"op": "unarchive_session", "session_id": sid},
                "session_archived",
                timeout=3.0,
            )
            assert unarch["session_id"] == sid and unarch["archived"] is False


@e2e_test(
    id="T1-DMN-11",
    tier=1,
    area="daemon",
    features=("F16",),
    title="Interrupt, steering & queued prompt cancellation on hosted session",
    baseline=False,
)
def test_slice2_interrupt_steer_and_cancel() -> None:
    with IsolatedHome(prefix="mb_t1_dmn11_") as sb:
        log_path = install_fake_agent_shims(sb)
        proj = sb.home / "repo"
        proj.mkdir(parents=True, exist_ok=True)
        with DaemonDriver(
            sb, use_fake_bin=True, extra_env={"FAKE_AGENT_SLOW": "1"}
        ) as dmn:
            spawned = dmn.request(
                {
                    "op": "spawn_session",
                    "agent": "antigravity",
                    "cwd": str(proj),
                    "title": "Slow Hosted Turn",
                    "initial_prompt": "Start long task",
                },
                "session_spawned",
                timeout=4.0,
            )
            sid = spawned["session"]["id"]
            boot_log = wait_for_log_substring(log_path, "agy:stdin:", timeout=3.0)
            assert "agy:stdin:" in boot_log, (
                f"Expected fake agy shim to start and receive initial prompt before interrupt; log={boot_log!r}"
            )

            pstat = dmn.request(
                {
                    "op": "steer_session",
                    "session_id": sid,
                    "agent": "antigravity",
                    "cwd": str(proj),
                    "text": "Switch priority to tests",
                },
                "prompt_status",
                timeout=3.0,
            )
            assert pstat["session_id"] == sid
            assert pstat.get("status") not in (None, "", "unsupported"), (
                f"Expected active steer/queue status from steer_session, got {pstat!r}"
            )

            inter = dmn.request(
                {"op": "interrupt_session", "session_id": sid, "agent": "antigravity"},
                "session_interrupted",
                timeout=3.0,
            )
            assert inter["session_id"] == sid and inter["ok"] is True
            log_text = wait_for_log_substring(log_path, "SIGINT", timeout=1.5)
            assert "SIGINT" in log_text, (
                f"Expected fake agy shim to record SIGINT from interrupt_session; log={log_text!r}"
            )


@e2e_test(
    id="T1-DMN-12",
    tier=1,
    area="daemon",
    features=("F18", "F19"),
    title="Bounded pagination, search_history & list_commands workspace skill discovery",
    baseline=False,
)
def test_slice2_pagination_search_and_list_commands() -> None:
    with IsolatedHome(prefix="mb_t1_dmn12_") as sb:
        large_path, _ = generate_large_claude_jsonl(
            sb.claude_projects_dir / "-tmp-proj-large" / "claude-large.jsonl",
            min_bytes=680 * 1024,
        )
        search_path = generate_search_claude_jsonl(
            sb.claude_projects_dir / "-tmp-proj-search" / "claude-search.jsonl",
            occurrences=60,
        )
        sb.stage_fixture(
            "skills/.claude/commands/deploy-vps.md",
            "repo/.claude/commands/deploy-vps.md",
        )
        sb.stage_fixture(
            "skills/.claude/skills/perf-audit/SKILL.md",
            "repo/.claude/skills/perf-audit/SKILL.md",
        )
        sb.stage_fixture(
            "skills/.codex/prompts/code-review.md",
            ".codex/prompts/code-review.md",
        )
        sb.stage_fixture(
            "skills/.agents/skills/security-check/SKILL.md",
            "repo/.agents/skills/security-check/SKILL.md",
        )

        with DaemonDriver(sb) as dmn:
            page1 = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-large",
                    "source_path": str(large_path),
                    "agent": "claude",
                    "max_bytes": 524288,
                },
                "history",
            )
            assert page1.get("has_more") is True, f"Expected has_more=True on >512KiB log: {page1.keys()}"
            assert int(page1.get("oldest_offset", 0)) > 0
            assert page1.get("probe_sha256"), "Expected non-empty probe_sha256"

            page2 = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-large",
                    "source_path": str(large_path),
                    "agent": "claude",
                    "before_offset": page1["oldest_offset"],
                    "max_bytes": 524288,
                },
                "history",
            )
            assert int(page2.get("oldest_offset", -1)) == 0
            assert page2.get("has_more") is False

            sr = dmn.request(
                {
                    "op": "search_history",
                    "request_id": "req-s1",
                    "session_id": "claude-search",
                    "source_path": str(search_path),
                    "agent": "claude",
                    "query": "needletoken",
                },
                "search_results",
            )
            assert sr["request_id"] == "req-s1" and len(sr["hits"]) == 60

            cmds_claude = dmn.request(
                {
                    "op": "list_commands",
                    "agent": "claude",
                    "cwd": str(sb.home / "repo"),
                },
                "commands",
            )
            claude_names = {str(item.get("name", "")) for item in cmds_claude.get("items", [])}
            assert any("deploy-vps" in n for n in claude_names), (
                f"Missing .claude/commands/deploy-vps for agent='claude' in {sorted(claude_names)}"
            )
            assert any("perf-audit" in n for n in claude_names), (
                f"Missing .claude/skills/perf-audit for agent='claude' in {sorted(claude_names)}"
            )

            cmds_agy = dmn.request(
                {
                    "op": "list_commands",
                    "agent": "antigravity",
                    "cwd": str(sb.home / "repo"),
                },
                "commands",
            )
            agy_names = {str(item.get("name", "")) for item in cmds_agy.get("items", [])}
            assert any("security-check" in n for n in agy_names), (
                f"Missing .agents/skills/security-check for agent='antigravity' in {sorted(agy_names)}"
            )

            cmds_codex = dmn.request(
                {
                    "op": "list_commands",
                    "agent": "codex",
                    "cwd": str(sb.home / "repo"),
                },
                "commands",
            )
            codex_names = {str(item.get("name", "")) for item in cmds_codex.get("items", [])}
            assert any("code-review" in n for n in codex_names), (
                f"Missing .codex/prompts/code-review for agent='codex' in {sorted(codex_names)}"
            )
            assert any("security-check" in n for n in codex_names), (
                f"Missing .agents/skills/security-check for agent='codex' in {sorted(codex_names)}"
            )
