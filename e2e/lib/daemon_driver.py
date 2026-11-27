"""Deadlock-free NDJSON subprocess driver for `multibotd --stdio`."""

from __future__ import annotations

import json
import os
import queue
import subprocess
import threading
import time
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Tuple, Union

from . import REPO_ROOT
from .sandbox import IsolatedHome


class DaemonError(AssertionError):
    """Raised when multibotd returns an unexpected error, EOF, or timeout."""


def resolve_multibotd_binary(explicit: Optional[Path] = None) -> Path:
    """Resolve the compiled multibotd binary without triggering implicit rebuilds."""
    candidates: List[Path] = []
    if explicit is not None:
        candidates.append(explicit)
    for env_key in ("E2E_MULTIBOTD_BIN", "MULTIBOTD_BIN"):
        val = os.environ.get(env_key)
        if val:
            candidates.append(Path(val))
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", str(REPO_ROOT / "target")))
    candidates.extend(
        [
            target_dir / "release" / "multibotd",
            target_dir / "debug" / "multibotd",
            REPO_ROOT / "target" / "release" / "multibotd",
            REPO_ROOT / "target" / "debug" / "multibotd",
        ]
    )
    for cand in candidates:
        if cand.is_file() and os.access(cand, os.X_OK):
            return cand
    raise FileNotFoundError(
        "multibotd binary not found. Checked: " + ", ".join(str(c) for c in candidates)
    )


def probe_cli_version(
    binary: Optional[Path] = None,
    *,
    timeout: float = 2.0,
) -> Tuple[int, str, str]:
    """Run `multibotd --version` safely with stdin=DEVNULL so Slice 1 never hangs."""
    bin_path = resolve_multibotd_binary(binary)
    try:
        proc = subprocess.run(
            [str(bin_path), "--version"],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        return proc.returncode, proc.stdout.strip(), proc.stderr.strip()
    except subprocess.TimeoutExpired as exc:
        raise DaemonError(
            f"`{bin_path} --version` timed out after {timeout:.1f}s "
            f"(daemon likely ignored --version and blocked on stdin)"
        ) from exc


class DaemonDriver:
    """Thread-backed NDJSON client for `multibotd --stdio` with guaranteed cleanup."""

    def __init__(
        self,
        sandbox: IsolatedHome,
        *,
        binary: Optional[Path] = None,
        use_fake_bin: bool = False,
        extra_env: Optional[Dict[str, str]] = None,
    ) -> None:
        self.sandbox = sandbox
        self.binary = resolve_multibotd_binary(binary)
        self.use_fake_bin = use_fake_bin
        self.extra_env = extra_env
        self._proc: Optional[subprocess.Popen[bytes]] = None
        self._events: "queue.Queue[Optional[Dict[str, Any]]]" = queue.Queue()
        self._stderr_lines: List[str] = []
        self._raw_stdout_errors: List[str] = []
        self._stdout_thread: Optional[threading.Thread] = None
        self._stderr_thread: Optional[threading.Thread] = None
        self._closed = False

    def __enter__(self) -> "DaemonDriver":
        return self.start()

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()

    def start(self) -> "DaemonDriver":
        if self._proc is not None:
            return self
        env = self.sandbox.build_env(
            use_fake_bin=self.use_fake_bin,
            extra_env=self.extra_env,
        )
        self._proc = subprocess.Popen(
            [str(self.binary), "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=-1,
            env=env,
            cwd=str(self.sandbox.home),
        )
        self._stdout_thread = threading.Thread(
            target=self._read_stdout,
            name="multibotd-stdout",
            daemon=True,
        )
        self._stderr_thread = threading.Thread(
            target=self._read_stderr,
            name="multibotd-stderr",
            daemon=True,
        )
        self._stdout_thread.start()
        self._stderr_thread.start()
        return self

    def _read_stdout(self) -> None:
        assert self._proc is not None and self._proc.stdout is not None
        stream = self._proc.stdout
        try:
            while True:
                try:
                    line = stream.readline()
                except (OSError, ValueError):
                    break
                if not line:
                    break
                text = line.decode("utf-8", errors="replace").strip()
                if not text:
                    continue
                try:
                    parsed = json.loads(text)
                    if isinstance(parsed, dict):
                        self._events.put(parsed)
                    else:
                        self._raw_stdout_errors.append(f"non-dict JSON: {text[:200]}")
                except json.JSONDecodeError as exc:
                    self._raw_stdout_errors.append(f"bad JSON ({exc}): {text[:200]}")
        finally:
            self._events.put(None)

    def _read_stderr(self) -> None:
        assert self._proc is not None and self._proc.stderr is not None
        stream = self._proc.stderr
        while True:
            try:
                line = stream.readline()
            except (OSError, ValueError):
                break
            if not line:
                break
            self._stderr_lines.append(line.decode("utf-8", errors="replace").rstrip("\n"))

    @property
    def stderr_text(self) -> str:
        return "\n".join(self._stderr_lines[-30:])

    def send(self, payload: Dict[str, Any]) -> None:
        """Encode payload as compact NDJSON line and flush to daemon stdin."""
        line = json.dumps(payload, separators=(",", ":"))
        self.send_raw(line)

    def send_raw(self, raw_line: str) -> None:
        """Write raw line + newline directly to daemon stdin."""
        if self._proc is None or self._proc.stdin is None:
            raise DaemonError("Daemon process is not running")
        try:
            self._proc.stdin.write((raw_line + "\n").encode("utf-8"))
            self._proc.stdin.flush()
        except BrokenPipeError as exc:
            raise DaemonError(
                f"Broken pipe writing to multibotd (stderr: {self.stderr_text})"
            ) from exc

    def next_event(self, timeout: float = 3.0) -> Dict[str, Any]:
        """Block up to timeout seconds for the next decoded ServerEvent."""
        try:
            item = self._events.get(timeout=timeout)
        except queue.Empty as exc:
            raise DaemonError(
                f"Timed out after {timeout:.2f}s waiting for daemon event. "
                f"stderr={self.stderr_text!r} raw_errors={self._raw_stdout_errors}"
            ) from exc
        if item is None:
            rc = self._proc.poll() if self._proc else None
            raise DaemonError(
                f"Daemon stdout reached EOF unexpectedly (rc={rc}). "
                f"stderr={self.stderr_text!r} raw_errors={self._raw_stdout_errors}"
            )
        return item

    @staticmethod
    def _format_seen(seen: List[Dict[str, Any]], max_items: int = 8, max_item_chars: int = 240) -> str:
        if not seen:
            return "[]"
        tail = seen[-max_items:]
        parts = []
        for ev in tail:
            s = repr(ev)
            if len(s) > max_item_chars:
                s = s[:max_item_chars] + "...<truncated>"
            parts.append(s)
        prefix = f"...({len(seen) - len(tail)} earlier) " if len(seen) > len(tail) else ""
        return f"[{prefix}{', '.join(parts)}]"

    def wait_for(
        self,
        event_type: Union[str, Tuple[str, ...]],
        *,
        predicate: Optional[Callable[[Dict[str, Any]], bool]] = None,
        timeout: float = 3.0,
        fail_on_bad_request: bool = True,
    ) -> Dict[str, Any]:
        """Wait for a matching ServerEvent, immediately failing on bad_request or not_implemented if enabled."""
        wanted: Tuple[str, ...] = (
            (event_type,) if isinstance(event_type, str) else tuple(event_type)
        )
        deadline = time.monotonic() + timeout
        seen: List[Dict[str, Any]] = []
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise DaemonError(
                    f"Timed out waiting for event={event_type!r} after {timeout:.2f}s. "
                    f"Seen={self._format_seen(seen)} stderr={self.stderr_text!r} "
                    f"raw_errors={self._raw_stdout_errors}"
                )
            try:
                ev = self._events.get(timeout=remaining)
            except queue.Empty as exc:
                raise DaemonError(
                    f"Timed out waiting for event={event_type!r} after {timeout:.2f}s. "
                    f"Seen={self._format_seen(seen)} stderr={self.stderr_text!r} "
                    f"raw_errors={self._raw_stdout_errors}"
                ) from exc
            if ev is None:
                rc = self._proc.poll() if self._proc else None
                raise DaemonError(
                    f"Daemon stdout reached EOF unexpectedly (rc={rc}) while awaiting event={event_type!r}. "
                    f"Seen={self._format_seen(seen)} stderr={self.stderr_text!r} "
                    f"raw_errors={self._raw_stdout_errors}"
                )
            if ev.get("event") in wanted:
                if predicate is None or predicate(ev):
                    return ev
            if (
                fail_on_bad_request
                and "error" not in wanted
                and ev.get("event") == "error"
                and ev.get("code") in ("bad_request", "not_implemented")
            ):
                code = ev.get("code")
                raise DaemonError(
                    f"Daemon rejected request with {code} while awaiting {event_type!r}: "
                    f"{ev.get('message', '')}"
                )
            seen.append(ev)

    def request(
        self,
        payload: Dict[str, Any],
        expected_event: Union[str, Tuple[str, ...]],
        *,
        predicate: Optional[Callable[[Dict[str, Any]], bool]] = None,
        timeout: float = 3.0,
        fail_on_bad_request: bool = True,
    ) -> Dict[str, Any]:
        """Send a request and wait for the expected ServerEvent."""
        self.send(payload)
        return self.wait_for(
            expected_event,
            predicate=predicate,
            timeout=timeout,
            fail_on_bad_request=fail_on_bad_request,
        )

    def expect_no_event(
        self,
        event_type: Optional[str] = None,
        *,
        duration: float = 0.30,
    ) -> List[Dict[str, Any]]:
        """Assert that no matching event arrives within duration seconds (event-driven queue wait)."""
        deadline = time.monotonic() + duration
        other_events: List[Dict[str, Any]] = []
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return other_events
            try:
                ev = self._events.get(timeout=remaining)
            except queue.Empty:
                return other_events
            if ev is None:
                return other_events
            if event_type is None or ev.get("event") == event_type:
                raise DaemonError(
                    f"Unexpected event arrived during quiet window ({duration:.2f}s): {ev!r}"
                )
            other_events.append(ev)

    def close(self) -> int:
        """Close stdin cleanly and reap multibotd so zero orphan processes remain."""
        if self._closed:
            return 0
        self._closed = True
        if self._proc is None:
            return 0
        try:
            if self._proc.stdin and not self._proc.stdin.closed:
                try:
                    self._proc.stdin.close()
                except OSError:
                    pass
            try:
                rc = self._proc.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                self._proc.terminate()
                try:
                    rc = self._proc.wait(timeout=1.5)
                except subprocess.TimeoutExpired:
                    self._proc.kill()
                    rc = self._proc.wait(timeout=1.5)
        finally:
            if self._stdout_thread and self._stdout_thread.is_alive():
                self._stdout_thread.join(timeout=1.0)
            if self._stderr_thread and self._stderr_thread.is_alive():
                self._stderr_thread.join(timeout=1.0)
            for stream in (self._proc.stdout, self._proc.stderr):
                if stream and not stream.closed:
                    try:
                        stream.close()
                    except OSError:
                        pass
        return rc
