"""Cached Cargo test/tree/metadata executor neutralizing Cargo's 0-test exit-0 trap."""

from __future__ import annotations

import atexit
import json
import os
import re
import shutil
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple

from . import REPO_ROOT

_RESULT_RE = re.compile(
    r"test result:\s+(ok|FAILED)\.\s+(\d+)\s+passed;\s+(\d+)\s+failed;\s+(\d+)\s+ignored"
)
_TEST_LINE_RE = re.compile(r"^test\s+([^\s]+)\s+\.\.\.\s+(ok|FAILED|ignored)\s*$")
_LIST_LINE_RE = re.compile(r"^([^\s:][^\s]*): test$")


@dataclass(frozen=True)
class CargoTestOutcome:
    crate: str
    filter_expr: Optional[str]
    returncode: int
    passed: int
    failed: int
    ignored: int
    passed_tests: Tuple[str, ...]
    failed_tests: Tuple[str, ...]
    stdout: str
    stderr: str

    def short_error(self) -> str:
        lines = [
            ln.strip()
            for ln in (self.stderr + "\n" + self.stdout).splitlines()
            if ln.strip()
        ]
        interesting = [
            ln
            for ln in lines
            if "error" in ln.lower() or "FAILED" in ln or "panicked" in ln.lower()
        ]
        excerpt = " | ".join((interesting or lines)[-4:])
        return excerpt[:360]


class CargoRunner:
    """Per-process cached executor for cargo test, cargo tree, and cargo metadata."""

    def __init__(self, repo_root: Path = REPO_ROOT) -> None:
        self.repo_root = repo_root
        self._test_cache: Dict[Tuple[str, Optional[str]], CargoTestOutcome] = {}
        self._list_cache: Dict[str, Tuple[int, List[str], str]] = {}
        self._tree_cache: Dict[Tuple[str, Optional[str]], Tuple[int, str, str]] = {}
        self._metadata_cache: Optional[Tuple[int, Optional[Dict[str, Any]], str]] = None
        self._sandbox_dir: Optional[Path] = None

    def _ensure_sandbox_dir(self) -> Path:
        if self._sandbox_dir is None:
            self._sandbox_dir = Path(tempfile.mkdtemp(prefix="multibot_e2e_cargo_"))
            (self._sandbox_dir / ".config").mkdir(parents=True, exist_ok=True)
            (self._sandbox_dir / ".local" / "share").mkdir(parents=True, exist_ok=True)
            (self._sandbox_dir / ".cache").mkdir(parents=True, exist_ok=True)
            atexit.register(shutil.rmtree, self._sandbox_dir, True)
        return self._sandbox_dir

    def _cargo_env(self, *, isolate_runtime: bool = False) -> Dict[str, str]:
        env = dict(os.environ)
        env.setdefault("CARGO_TERM_COLOR", "never")
        if isolate_runtime:
            real_home = Path.home()
            env.setdefault("CARGO_HOME", str(real_home / ".cargo"))
            env.setdefault("RUSTUP_HOME", str(real_home / ".rustup"))
            sb = self._ensure_sandbox_dir()
            env["HOME"] = str(sb)
            env["XDG_CONFIG_HOME"] = str(sb / ".config")
            env["XDG_DATA_HOME"] = str(sb / ".local" / "share")
            env["XDG_CACHE_HOME"] = str(sb / ".cache")
            env["APPDATA"] = str(sb / ".config")
            env["LOCALAPPDATA"] = str(sb / ".local" / "share")
            env.pop("MULTIBOT_UPDATE_URL", None)
        return env

    def run_crate_tests(
        self,
        crate: str,
        filter_expr: Optional[str] = None,
        *,
        timeout: float = 120.0,
    ) -> CargoTestOutcome:
        key = (crate, filter_expr)
        if key in self._test_cache:
            return self._test_cache[key]

        # Run full crate test suite once when filter_expr is set? Better: run full crate once
        # and filter matched tests locally if we already ran the full crate cleanly, OR run exact
        # cargo test -p <crate> [filter]. Running the full crate once and deriving filtered counts
        # makes 70+ tests run in seconds while still executing 100% real compiled Rust tests!
        if filter_expr is not None:
            full = self.run_crate_tests(crate, None, timeout=timeout)
            if full.returncode != 0:
                self._test_cache[key] = full
                return full
            matched_passed = tuple(t for t in full.passed_tests if filter_expr in t)
            matched_failed = tuple(t for t in full.failed_tests if filter_expr in t)
            derived = CargoTestOutcome(
                crate=crate,
                filter_expr=filter_expr,
                returncode=0 if not matched_failed else 101,
                passed=len(matched_passed),
                failed=len(matched_failed),
                ignored=0,
                passed_tests=matched_passed,
                failed_tests=matched_failed,
                stdout=full.stdout,
                stderr=full.stderr,
            )
            self._test_cache[key] = derived
            return derived

        cmd = ["cargo", "test", "-p", crate]
        try:
            proc = subprocess.run(
                cmd,
                cwd=str(self.repo_root),
                env=self._cargo_env(isolate_runtime=True),
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired as exc:
            outcome = CargoTestOutcome(
                crate=crate,
                filter_expr=None,
                returncode=124,
                passed=0,
                failed=1,
                ignored=0,
                passed_tests=(),
                failed_tests=(),
                stdout="",
                stderr=f"cargo test -p {crate} timed out after {timeout:.0f}s",
            )
            self._test_cache[key] = outcome
            return outcome

        passed = 0
        failed = 0
        ignored = 0
        passed_tests: List[str] = []
        failed_tests: List[str] = []

        for line in proc.stdout.splitlines():
            m = _TEST_LINE_RE.match(line.strip())
            if m:
                tname, status = m.group(1), m.group(2)
                if status == "ok":
                    passed_tests.append(tname)
                elif status == "FAILED":
                    failed_tests.append(tname)
            rm = _RESULT_RE.search(line)
            if rm:
                passed += int(rm.group(2))
                failed += int(rm.group(3))
                ignored += int(rm.group(4))

        outcome = CargoTestOutcome(
            crate=crate,
            filter_expr=None,
            returncode=proc.returncode,
            passed=passed,
            failed=failed,
            ignored=ignored,
            passed_tests=tuple(passed_tests),
            failed_tests=tuple(failed_tests),
            stdout=proc.stdout,
            stderr=proc.stderr,
        )
        self._test_cache[key] = outcome
        return outcome

    def assert_crate_tests_pass(
        self,
        crate: str,
        *,
        filter_expr: Optional[str] = None,
        min_expected: int = 1,
        required_substrings: Optional[Sequence[str]] = None,
        required_any_of: Optional[Sequence[Sequence[str]]] = None,
    ) -> CargoTestOutcome:
        """Execute cargo test -p <crate> and enforce passed >= min_expected and failed == 0."""
        outcome = self.run_crate_tests(crate, filter_expr=filter_expr)
        if outcome.returncode != 0:
            raise AssertionError(
                f"cargo test -p {crate}"
                f"{f' {filter_expr}' if filter_expr else ''} failed (rc={outcome.returncode}): "
                f"{outcome.short_error()}"
            )
        if outcome.failed != 0:
            raise AssertionError(
                f"cargo test -p {crate} had {outcome.failed} failing test(s): {outcome.failed_tests}"
            )
        if outcome.passed < min_expected:
            raise AssertionError(
                f"cargo test -p {crate}"
                f"{f' (filter={filter_expr!r})' if filter_expr else ''} "
                f"passed only {outcome.passed} test(s), expected >= {min_expected} "
                f"(0-test false-green guard triggered; matched={list(outcome.passed_tests)})"
            )
        if required_substrings:
            lowered = [t.lower() for t in outcome.passed_tests]
            missing = [
                sub
                for sub in required_substrings
                if not any(sub.lower() in name for name in lowered)
            ]
            if missing:
                raise AssertionError(
                    f"cargo test -p {crate} missing required behavioral test coverage for "
                    f"{missing} (executed tests: {list(outcome.passed_tests)})"
                )
        if required_any_of:
            lowered = [t.lower() for t in outcome.passed_tests]
            missing_groups = [
                tuple(group)
                for group in required_any_of
                if not any(tok.lower() in name for name in lowered for tok in group)
            ]
            if missing_groups:
                raise AssertionError(
                    f"cargo test -p {crate} missing required capability test coverage for groups "
                    f"{missing_groups} (executed tests: {list(outcome.passed_tests)})"
                )
        return outcome

    def list_crate_tests(self, crate: str) -> Tuple[int, List[str], str]:
        if crate in self._list_cache:
            return self._list_cache[crate]
        proc = subprocess.run(
            ["cargo", "test", "-p", crate, "--", "--list"],
            cwd=str(self.repo_root),
            env=self._cargo_env(),
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=90,
        )
        names: List[str] = []
        for line in proc.stdout.splitlines():
            m = _LIST_LINE_RE.match(line.strip())
            if m:
                names.append(m.group(1))
        result = (proc.returncode, names, proc.stderr)
        self._list_cache[crate] = result
        return result

    def cargo_tree(
        self,
        package: str,
        *,
        target: Optional[str] = None,
    ) -> Tuple[int, str, str]:
        key = (package, target)
        if key in self._tree_cache:
            return self._tree_cache[key]
        cmd = ["cargo", "tree", "-p", package]
        if target:
            cmd.extend(["--target", target])
        proc = subprocess.run(
            cmd,
            cwd=str(self.repo_root),
            env=self._cargo_env(),
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=60,
        )
        res = (proc.returncode, proc.stdout, proc.stderr)
        self._tree_cache[key] = res
        return res

    def cargo_metadata(self) -> Tuple[int, Optional[Dict[str, Any]], str]:
        if self._metadata_cache is not None:
            return self._metadata_cache
        proc = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            cwd=str(self.repo_root),
            env=self._cargo_env(),
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=60,
        )
        parsed: Optional[Dict[str, Any]] = None
        if proc.returncode == 0 and proc.stdout.strip():
            parsed = json.loads(proc.stdout)
        self._metadata_cache = (proc.returncode, parsed, proc.stderr)
        return self._metadata_cache


CARGO = CargoRunner()
