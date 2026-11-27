"""Deterministic generators for large JSONL fixtures and offline fake agent CLI shims."""

from __future__ import annotations

import hashlib
import json
import threading
import time
from pathlib import Path
from typing import Tuple

from .sandbox import IsolatedHome


def antigravity_history_filename(session_id: str) -> str:
    """Compute canonical daemon-owned history filename <sha256('antigravity:'+sid)[:32]>.history.jsonl."""
    digest = hashlib.sha256(f"antigravity:{session_id}".encode("utf-8")).hexdigest()[:32]
    return f"{digest}.history.jsonl"


def generate_large_claude_jsonl(
    dst: Path,
    *,
    min_bytes: int = 700 * 1024,
    cwd: str = "/tmp/proj/large",
) -> Tuple[Path, int]:
    """Generate a deterministic >512 KiB Claude JSONL transcript for pagination tests."""
    dst.parent.mkdir(parents=True, exist_ok=True)
    lines: list[str] = []
    total_bytes = 0
    turn = 0
    pad = "deterministic-payload-chunk-" * 18  # ~504 bytes per message
    while total_bytes < min_bytes:
        turn += 1
        u_line = json.dumps(
            {
                "type": "user",
                "uuid": f"u-page-{turn:04d}",
                "cwd": cwd,
                "timestamp": f"2026-11-26T10:{(turn // 60) % 60:02d}:{turn % 60:02d}Z",
                "message": {"content": f"TURN_MARKER_{turn:04d} user request {pad}"},
            },
            separators=(",", ":"),
        )
        a_line = json.dumps(
            {
                "type": "assistant",
                "uuid": f"a-page-{turn:04d}",
                "cwd": cwd,
                "timestamp": f"2026-11-26T10:{(turn // 60) % 60:02d}:{turn % 60:02d}Z",
                "message": {
                    "model": "claude-opus-4-6",
                    "content": [
                        {
                            "type": "text",
                            "text": f"REPLY_MARKER_{turn:04d} assistant answer {pad}",
                        }
                    ],
                },
            },
            separators=(",", ":"),
        )
        lines.append(u_line)
        lines.append(a_line)
        total_bytes += len(u_line) + len(a_line) + 2

    dst.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return dst, turn


def generate_search_claude_jsonl(
    dst: Path,
    *,
    occurrences: int = 600,
    token: str = "NeedleToken",
    early_unique_token: str = "ANCIENT_ARCHIVE_NEEDLE_0001",
    cwd: str = "/tmp/proj/search",
) -> Path:
    """Generate a deterministic Claude JSONL with 600 mixed-case occurrences + an early unique hit."""
    dst.parent.mkdir(parents=True, exist_ok=True)
    lines: list[str] = []

    # Place an early unique token beyond the 512 KiB tail for Tier 3 pairwise search+pagination
    lines.append(
        json.dumps(
            {
                "type": "user",
                "uuid": "u-ancient-0001",
                "cwd": cwd,
                "timestamp": "2026-11-26T08:00:00Z",
                "message": {"content": f"Locate {early_unique_token} from early history"},
            },
            separators=(",", ":"),
        )
    )

    for idx in range(1, occurrences + 1):
        variant = token if idx % 2 == 0 else token.upper()
        pad = " filler-context-padding" * 35
        lines.append(
            json.dumps(
                {
                    "type": "assistant",
                    "uuid": f"a-hit-{idx:04d}",
                    "cwd": cwd,
                    "timestamp": f"2026-11-26T09:{(idx // 60) % 60:02d}:{idx % 60:02d}Z",
                    "message": {
                        "content": [
                            {
                                "type": "text",
                                "text": f"Hit #{idx:04d} contains {variant} here.{pad}",
                            }
                        ]
                    },
                },
                separators=(",", ":"),
            )
        )

    dst.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return dst


def generate_base64_and_long_tool_jsonl(
    dst: Path,
    *,
    cwd: str = "/tmp/proj/b64",
) -> Path:
    """Generate a Claude JSONL containing a 500 KiB base64 PNG blob + 20,000-char tool detail."""
    dst.parent.mkdir(parents=True, exist_ok=True)
    huge_b64 = "data:image/png;base64," + ("A" * (500 * 1024)) + "=="
    huge_text = "LONG_OUTPUT_LINE_" + ("X" * 20000)

    records = [
        {
            "type": "user",
            "uuid": "u-b64-1",
            "cwd": cwd,
            "timestamp": "2026-11-26T10:00:00Z",
            "message": {"content": "Capture screenshot and dump log"},
        },
        {
            "type": "assistant",
            "uuid": "a-b64-1",
            "cwd": cwd,
            "timestamp": "2026-11-26T10:00:01Z",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "tu_img_1",
                        "name": "Bash",
                        "input": {"command": "capture_png", "description": "Capture PNG"},
                    },
                    {
                        "type": "tool_use",
                        "id": "tu_long_1",
                        "name": "Bash",
                        "input": {"command": "dump_log", "description": "Dump verbose log"},
                    },
                ]
            },
        },
        {
            "type": "user",
            "uuid": "u-b64-res",
            "cwd": cwd,
            "timestamp": "2026-11-26T10:00:02Z",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "tu_img_1",
                        "is_error": False,
                        "content": huge_b64,
                    },
                    {
                        "type": "tool_result",
                        "tool_use_id": "tu_long_1",
                        "is_error": False,
                        "content": huge_text,
                    },
                ]
            },
        },
        {
            "type": "assistant",
            "uuid": "a-b64-2",
            "cwd": cwd,
            "timestamp": "2026-11-26T10:00:03Z",
            "message": {"content": [{"type": "text", "text": "Captured and dumped."}]},
        },
    ]
    dst.write_text(
        "\n".join(json.dumps(r, separators=(",", ":")) for r in records) + "\n",
        encoding="utf-8",
    )
    return dst


def install_fake_agent_shims(sandbox: IsolatedHome) -> Path:
    """Install offline deterministic fake `agy`, `claude`, and `codex` CLI shims.

    Returns the log file path where every invocation and received SIGINT is recorded.
    """
    log_path = sandbox.root / "fake_agents.log"

    fake_agy = f"""#!/usr/bin/env python3
import json, os, select, signal, sys, threading

LOG = {str(log_path)!r}
stop_evt = threading.Event()

def record(msg):
    with open(LOG, "a", encoding="utf-8") as f:
        f.write(msg + "\\n")

def on_sigint(signum, frame):
    record("agy:SIGINT")
    print(json.dumps({{"event": "result", "status": "CANCELED"}}), flush=True)
    stop_evt.set()
    sys.exit(130)

signal.signal(signal.SIGINT, on_sigint)
record("agy:start:" + " ".join(sys.argv[1:]))
conv_id = os.environ.get("FAKE_AGY_CONV_ID", "agy-live-001")
print(json.dumps({{"event": "init", "conversation_id": conv_id}}), flush=True)

slow = os.environ.get("FAKE_AGENT_SLOW", "0") == "1"

for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    record("agy:stdin:" + line)
    if slow:
        print(json.dumps({{"event": "step_update", "kind": "agent_response", "text_delta": "Working..."}}), flush=True)
        stop_evt.wait(timeout=8.0)
        break
    print(json.dumps({{"event": "step_update", "kind": "agent_response", "text_delta": "Fake agy reply"}}), flush=True)
    print(json.dumps({{"event": "result", "status": "SUCCESS"}}), flush=True)
"""

    fake_claude = f"""#!/usr/bin/env python3
import json, os, signal, sys, threading

LOG = {str(log_path)!r}
stop_evt = threading.Event()

def record(msg):
    with open(LOG, "a", encoding="utf-8") as f:
        f.write(msg + "\\n")

def on_sigint(signum, frame):
    record("claude:SIGINT")
    stop_evt.set()
    sys.exit(130)

signal.signal(signal.SIGINT, on_sigint)
record("claude:start:" + " ".join(sys.argv[1:]))
slow = os.environ.get("FAKE_AGENT_SLOW", "0") == "1"

if "--output-format" in sys.argv and "stream-json" in sys.argv:
    print(json.dumps({{"type": "system", "subtype": "init", "session_id": "claude-live-001"}}), flush=True)
    if slow:
        stop_evt.wait(timeout=8.0)
    else:
        print(json.dumps({{"type": "assistant", "message": {{"content": [{{"type": "text", "text": "Fake claude reply"}}]}}}}), flush=True)
        print(json.dumps({{"type": "result", "subtype": "success"}}), flush=True)
else:
    if slow:
        stop_evt.wait(timeout=8.0)
    print("Fake claude CLI ok", flush=True)
"""

    fake_codex = f"""#!/usr/bin/env python3
import json, os, signal, sys, threading

LOG = {str(log_path)!r}
stop_evt = threading.Event()

def record(msg):
    with open(LOG, "a", encoding="utf-8") as f:
        f.write(msg + "\\n")

def on_sigint(signum, frame):
    record("codex:SIGINT")
    stop_evt.set()
    sys.exit(130)

signal.signal(signal.SIGINT, on_sigint)
record("codex:start:" + " ".join(sys.argv[1:]))
if os.environ.get("FAKE_AGENT_SLOW", "0") == "1":
    stop_evt.wait(timeout=8.0)
print(json.dumps({{"type": "turn.completed", "text": "Fake codex reply"}}), flush=True)
"""

    sandbox.install_fake_cli("agy", fake_agy)
    sandbox.install_fake_cli("claude", fake_claude)
    sandbox.install_fake_cli("codex", fake_codex)
    return log_path


def wait_for_log_substring(
    log_path: Path,
    needle: str,
    *,
    timeout: float = 1.5,
    poll_interval: float = 0.02,
) -> str:
    """Bounded event-driven wait for needle to appear in a fake-agent log file.

    Returns the log contents as soon as needle is present, or the last read
    contents once timeout expires.
    """
    waiter = threading.Event()
    deadline = time.monotonic() + timeout
    content = ""
    while True:
        if log_path.is_file():
            content = log_path.read_text(encoding="utf-8", errors="replace")
            if needle in content:
                return content
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return content
        waiter.wait(min(poll_interval, remaining))
