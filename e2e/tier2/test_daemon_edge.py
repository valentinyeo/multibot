"""Tier 2 multibotd boundary & edge tests: Slice 1 baseline (T2-DMN-01..03) + Slice 2 (T2-DMN-04..10)."""

from __future__ import annotations

import json
import os

from e2e.lib.daemon_driver import DaemonDriver
from e2e.lib.fixture_gen import (
    generate_base64_and_long_tool_jsonl,
    generate_large_claude_jsonl,
    generate_search_claude_jsonl,
)
from e2e.lib.runner import e2e_test
from e2e.lib.sandbox import IsolatedHome


# ---------------------------------------------------------------------------
# Slice 1 Baseline Boundary Tests (T2-DMN-01..03) — MUST PASS IMMEDIATELY
# ---------------------------------------------------------------------------


@e2e_test(
    id="T2-DMN-01",
    tier=2,
    area="daemon",
    features=("F1",),
    title="Slice 1 malformed JSON, blank lines & unknown op return bad_request without crashing daemon",
    baseline=True,
)
def test_slice1_malformed_json_and_unknown_op() -> None:
    with IsolatedHome(prefix="mb_t2_dmn01_") as sb:
        with DaemonDriver(sb) as dmn:
            dmn.send_raw("{broken json")
            err1 = dmn.wait_for("error", fail_on_bad_request=False)
            assert err1["code"] == "bad_request"

            dmn.send_raw("")
            dmn.send_raw("   ")
            dmn.send({"op": "nonexistent_op_xyz"})
            err2 = dmn.wait_for("error", fail_on_bad_request=False)
            assert err2["code"] == "bad_request"

            pong = dmn.request({"op": "ping", "nonce": 555}, "pong")
            assert pong["nonce"] == 555


@e2e_test(
    id="T2-DMN-02",
    tier=2,
    area="daemon",
    features=("F1", "F14"),
    title="Slice 1 empty HOME returns [], missing paths return read_history_failed / watch_failed",
    baseline=True,
)
def test_slice1_empty_home_and_missing_paths() -> None:
    with IsolatedHome(prefix="mb_t2_dmn02_") as sb:
        with DaemonDriver(sb) as dmn:
            sess = dmn.request({"op": "list_sessions", "limit": 20}, "sessions")
            assert sess["sessions"] == []

            missing = str(sb.home / "nonexistent-dir" / "does-not-exist.jsonl")
            err_r = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "missing",
                    "source_path": missing,
                    "agent": "claude",
                },
                "error",
                fail_on_bad_request=False,
            )
            assert err_r["code"] == "read_history_failed"

            err_w = dmn.request(
                {
                    "op": "watch_session",
                    "session_id": "missing",
                    "source_path": missing,
                    "agent": "claude",
                },
                "error",
                fail_on_bad_request=False,
            )
            assert err_w["code"] == "watch_failed"


@e2e_test(
    id="T2-DMN-03",
    tier=2,
    area="daemon",
    features=("F11",),
    title="Slice 1 list_sessions limit=0 returns [] and limit=2 returns 2 newest sessions",
    baseline=True,
)
def test_slice1_list_sessions_limit_bounds() -> None:
    with IsolatedHome(prefix="mb_t2_dmn03_") as sb:
        for i in range(5):
            sb.stage_fixture(
                "claude/basic_slice1.jsonl",
                f".claude/projects/-tmp-lim/sess-{i}.jsonl",
                mtime_offset_sec=-(50 - i * 10),
            )
        with DaemonDriver(sb) as dmn:
            s2 = dmn.request({"op": "list_sessions", "limit": 2}, "sessions")["sessions"]
            assert len(s2) == 2
            assert s2[0]["id"] == "sess-4" and s2[1]["id"] == "sess-3", (
                f"Expected 2 newest sessions [sess-4, sess-3], got {[s['id'] for s in s2]}"
            )
            s0 = dmn.request({"op": "list_sessions", "limit": 0}, "sessions")["sessions"]
            assert s0 == []


# ---------------------------------------------------------------------------
# Slice 2 Boundary Tests (T2-DMN-04..10) — Fail Cleanly Until M3
# ---------------------------------------------------------------------------


@e2e_test(
    id="T2-DMN-04",
    tier=2,
    area="daemon",
    features=("F14",),
    title="Partial trailing JSONL line waits for newline on inotify without corrupting or skipping",
    baseline=False,
)
def test_slice2_partial_trailing_jsonl_line() -> None:
    with IsolatedHome(prefix="mb_t2_dmn04_") as sb:
        c_path = sb.stage_fixture(
            "claude/basic_slice1.jsonl",
            ".claude/projects/-tmp-partial/claude-partial.jsonl",
        )
        with DaemonDriver(sb) as dmn:
            dmn.request(
                {
                    "op": "watch_session",
                    "session_id": "claude-partial",
                    "source_path": str(c_path),
                    "agent": "claude",
                },
                "history",
            )

            part1 = '{"type":"assistant","uuid":"a-split-1","timestamp":"2026-11-26T10:20:00Z","message":{"content":[{"type":"text","text":"SPLIT_LINE_'
            part2 = 'DELIVERED_CLEANLY"}]}}\n'

            with c_path.open("a", encoding="utf-8") as fh:
                fh.write(part1)
                fh.flush()
                os.fsync(fh.fileno())

            # Must not emit bogus/corrupt append before newline arrives
            dmn.expect_no_event("history_append", duration=0.25)

            with c_path.open("a", encoding="utf-8") as fh:
                fh.write(part2)
                fh.flush()
                os.fsync(fh.fileno())

            app = dmn.wait_for(
                "history_append",
                predicate=lambda ev: ev.get("session_id") == "claude-partial",
                timeout=2.0,
            )
            assert "SPLIT_LINE_DELIVERED_CLEANLY" in json.dumps(app), (
                f"Partial trailing line was dropped or corrupted: {app!r}"
            )


@e2e_test(
    id="T2-DMN-05",
    tier=2,
    area="daemon",
    features=("F14",),
    title="File truncation / rotation during watch_session resets offset & streams post-rotation turns",
    baseline=False,
)
def test_slice2_file_truncation_resets_watch_offset() -> None:
    with IsolatedHome(prefix="mb_t2_dmn05_") as sb:
        c_path = sb.stage_fixture(
            "claude/rich_slice2.jsonl",
            ".claude/projects/-tmp-trunc/claude-trunc.jsonl",
        )
        with DaemonDriver(sb) as dmn:
            init_h = dmn.request(
                {
                    "op": "watch_session",
                    "session_id": "claude-trunc",
                    "source_path": str(c_path),
                    "agent": "claude",
                },
                "history",
            )
            init_probe = init_h.get("probe_sha256")

            # Truncate file far below previous offset and write one short turn
            fresh = json.dumps(
                {
                    "type": "assistant",
                    "uuid": "a-post-rot",
                    "timestamp": "2026-11-26T10:30:00Z",
                    "message": {
                        "content": [{"type": "text", "text": "POST_TRUNCATION_RECOVERY_OK"}]
                    },
                }
            )
            with c_path.open("w", encoding="utf-8") as fh:
                fh.write(fresh + "\n")
                fh.flush()
                os.fsync(fh.fileno())

            ev = dmn.wait_for(
                ("history", "history_append"),
                predicate=lambda e: e.get("session_id") == "claude-trunc"
                and "POST_TRUNCATION_RECOVERY_OK" in json.dumps(e),
                timeout=3.0,
            )
            assert "POST_TRUNCATION_RECOVERY_OK" in json.dumps(ev), (
                f"Expected POST_TRUNCATION_RECOVERY_OK after truncation/rotation: {ev!r}"
            )
            assert ev.get("probe_sha256"), (
                f"Expected updated probe_sha256 after truncation/rotation: {ev!r}"
            )
            if init_probe:
                assert ev.get("probe_sha256") != init_probe, (
                    f"Expected probe_sha256 to change after truncation/rotation; "
                    f"before={init_probe!r}, after={ev.get('probe_sha256')!r}"
                )


@e2e_test(
    id="T2-DMN-06",
    tier=2,
    area="daemon",
    features=("F11", "F1"),
    title="500 KiB base64 PNG blob stripped to [image omitted] & tool output capped at 4000 chars",
    baseline=False,
)
def test_slice2_base64_strip_and_4000_char_cap() -> None:
    with IsolatedHome(prefix="mb_t2_dmn06_") as sb:
        b_path = generate_base64_and_long_tool_jsonl(
            sb.claude_projects_dir / "-tmp-b64" / "claude-b64.jsonl"
        )
        with DaemonDriver(sb) as dmn:
            hist = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-b64",
                    "source_path": str(b_path),
                    "agent": "claude",
                    "max_bytes": 2 * 1024 * 1024,
                },
                "history",
            )
            raw_wire = json.dumps(hist)
            assert "A" * 256 not in raw_wire, "Raw base64 PNG data leaked into history event!"
            assert "[image omitted]" in raw_wire, (
                "Expected base64 image payload to be replaced with '[image omitted]'"
            )
            assert len(raw_wire) < 40 * 1024, (
                f"History wire payload unexpectedly huge ({len(raw_wire)} bytes) — capping failed"
            )
            for msg in hist["messages"]:
                for blk in msg.get("blocks") or []:
                    body = next(iter(blk.values())) if len(blk) == 1 and isinstance(next(iter(blk.values())), dict) else blk
                    for field in ("detail", "output"):
                        val = body.get(field)
                        if isinstance(val, str):
                            assert len(val) <= 4100, (
                                f"Tool {field} exceeded 4000-char cap (got {len(val)} chars)"
                            )


@e2e_test(
    id="T2-DMN-07",
    tier=2,
    area="daemon",
    features=("F11",),
    title="Claude isMeta, isSidechain, isCompactSummary, isVisibleInTranscriptOnly & malformed lines filtered",
    baseline=False,
)
def test_slice2_claude_meta_sidechain_and_malformed_filtered() -> None:
    with IsolatedHome(prefix="mb_t2_dmn07_") as sb:
        c_path = sb.stage_fixture(
            "claude/rich_slice2.jsonl",
            ".claude/projects/-tmp-rich/claude-rich-001.jsonl",
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
            dump = json.dumps(hist["messages"])
            for sentinel in (
                "META_SHOULD_NOT_APPEAR",
                "SIDECHAIN_SHOULD_NOT_APPEAR",
                "COMPACT_SHOULD_NOT_APPEAR",
                "TRANSCRIPT_ONLY_SHOULD_NOT_APPEAR",
                "<task-notification>",
                "[Request interrupted by user]",
            ):
                assert sentinel not in dump, (
                    f"Filtered Claude metadata/noise sentinel leaked into history: {sentinel!r}"
                )
            assert "Running tests and inspecting the engine." in dump


@e2e_test(
    id="T2-DMN-08",
    tier=2,
    area="daemon",
    features=("F15",),
    title="Semantic working state overrides naive <8s mtime heuristic (done fresh=false, open tool=true)",
    baseline=False,
)
def test_slice2_semantic_working_state_vs_mtime() -> None:
    with IsolatedHome(prefix="mb_t2_dmn08_") as sb:
        sb.stage_fixture(
            "claude/semantic_done_fresh.jsonl",
            ".claude/projects/-tmp-sem/done-fresh.jsonl",
            mtime_offset_sec=0.0,
        )
        busy_path = sb.stage_fixture(
            "claude/semantic_busy_open_tool.jsonl",
            ".claude/projects/-tmp-sem/busy-open.jsonl",
            mtime_offset_sec=-15.0,
        )
        with DaemonDriver(sb) as dmn:
            sess = dmn.request({"op": "list_sessions", "limit": 20}, "sessions")["sessions"]
            by_id = {s["id"]: s for s in sess}
            assert "done-fresh" in by_id and "busy-open" in by_id
            assert by_id["done-fresh"]["working"] is False, (
                f"Freshly written completed assistant turn must report working=False, got {by_id['done-fresh']!r}"
            )
            assert by_id["busy-open"]["working"] is True, (
                f"Open unanswered tool_use (15s old, beyond 8s naive mtime cutoff and within 30s staleness guard) "
                f"must report working=True, got {by_id['busy-open']!r}"
            )

            # Complete the open tool and append assistant completion -> working must flip False
            with busy_path.open("a", encoding="utf-8") as fh:
                fh.write(
                    json.dumps(
                        {
                            "type": "user",
                            "uuid": "u-busy-res",
                            "timestamp": "2026-11-26T11:02:00Z",
                            "message": {
                                "content": [
                                    {
                                        "type": "tool_result",
                                        "tool_use_id": "tu_open_1",
                                        "is_error": False,
                                        "content": "ok",
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
                            "uuid": "a-busy-done",
                            "timestamp": "2026-11-26T11:02:01Z",
                            "message": {
                                "content": [{"type": "text", "text": "Tests finished."}]
                            },
                        }
                    )
                    + "\n"
                )
                fh.flush()

            sess2 = dmn.request({"op": "list_sessions", "limit": 20}, "sessions")["sessions"]
            by_id2 = {s["id"]: s for s in sess2}
            assert by_id2["busy-open"]["working"] is False, (
                f"After completing open tool_use, working must flip to False: {by_id2['busy-open']!r}"
            )


@e2e_test(
    id="T2-DMN-09",
    tier=2,
    area="daemon",
    features=("F18",),
    title="512 KiB tail pagination, non-overlapping older page & probe_sha256 mismatch reload",
    baseline=False,
)
def test_slice2_pagination_and_probe_mismatch() -> None:
    with IsolatedHome(prefix="mb_t2_dmn09_") as sb:
        large_path, _ = generate_large_claude_jsonl(
            sb.claude_projects_dir / "-tmp-page" / "claude-page.jsonl",
            min_bytes=720 * 1024,
        )
        with DaemonDriver(sb) as dmn:
            p1 = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-page",
                    "source_path": str(large_path),
                    "agent": "claude",
                    "max_bytes": 524288,
                },
                "history",
            )
            assert p1.get("has_more") is True, (
                f"Expected has_more=True for >512 KiB transcript, got {p1.get('has_more')!r}"
            )
            assert int(p1.get("oldest_offset", 0)) > 0, (
                f"Expected oldest_offset > 0 for bounded tail page, got {p1.get('oldest_offset')!r}"
            )
            assert int(p1.get("latest_offset", 0)) == large_path.stat().st_size, (
                f"Expected latest_offset == {large_path.stat().st_size}, got {p1.get('latest_offset')!r}"
            )
            probe = str(p1.get("probe_sha256", ""))
            assert len(probe) >= 32, f"Expected valid probe_sha256, got {p1!r}"

            p2 = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-page",
                    "source_path": str(large_path),
                    "agent": "claude",
                    "before_offset": p1["oldest_offset"],
                    "max_bytes": 524288,
                },
                "history",
            )
            ids1 = {m["id"] for m in p1["messages"]}
            ids2 = {m["id"] for m in p2["messages"]}
            assert ids2 and ids1.isdisjoint(ids2), (
                f"Older page overlapped with tail page: overlap={len(ids1 & ids2)}"
            )

            # Passing mismatched known_probe must still return fresh valid tail + current probe
            p_mismatch = dmn.request(
                {
                    "op": "read_history",
                    "session_id": "claude-page",
                    "source_path": str(large_path),
                    "agent": "claude",
                    "max_bytes": 524288,
                    "known_probe": "0" * 64,
                },
                "history",
            )
            assert p_mismatch.get("probe_sha256") == probe and len(p_mismatch["messages"]) > 0


@e2e_test(
    id="T2-DMN-10",
    tier=2,
    area="daemon",
    features=("F18",),
    title="search_history query validation (empty / >200 chars rejected) & 500-hit truncation cap",
    baseline=False,
)
def test_slice2_search_history_validation_and_500_cap() -> None:
    with IsolatedHome(prefix="mb_t2_dmn10_") as sb:
        s_path = generate_search_claude_jsonl(
            sb.claude_projects_dir / "-tmp-search" / "claude-search-600.jsonl",
            occurrences=600,
        )
        with DaemonDriver(sb) as dmn:
            sr = dmn.request(
                {
                    "op": "search_history",
                    "request_id": "req-cap",
                    "session_id": "claude-search-600",
                    "source_path": str(s_path),
                    "agent": "claude",
                    "query": "needletoken",
                },
                "search_results",
            )
            assert len(sr["hits"]) == 500 and sr["truncated"] is True, (
                f"Expected 500-hit cap with truncated=True, got len={len(sr.get('hits', []))}, truncated={sr.get('truncated')}"
            )

            sr_none = dmn.request(
                {
                    "op": "search_history",
                    "request_id": "req-none",
                    "session_id": "claude-search-600",
                    "source_path": str(s_path),
                    "agent": "claude",
                    "query": "ZZZ_NO_MATCH_999",
                },
                "search_results",
            )
            assert sr_none["hits"] == [] and sr_none["truncated"] is False

            err_empty = dmn.request(
                {
                    "op": "search_history",
                    "request_id": "req-empty",
                    "session_id": "claude-search-600",
                    "source_path": str(s_path),
                    "agent": "claude",
                    "query": "",
                },
                "error",
                fail_on_bad_request=False,
            )
            assert err_empty["code"] in ("invalid_query", "bad_request")

            err_long = dmn.request(
                {
                    "op": "search_history",
                    "request_id": "req-long",
                    "session_id": "claude-search-600",
                    "source_path": str(s_path),
                    "agent": "claude",
                    "query": "x" * 201,
                },
                "error",
                fail_on_bad_request=False,
            )
            assert err_long["code"] in ("invalid_query", "bad_request")
