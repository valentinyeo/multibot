"""Opaque-box E2E test registry, timeout guard, summary table, and JSON reporter."""

from __future__ import annotations

import json
import os
import re
import signal
import sys
import time
import traceback
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Callable, Dict, List, Optional, Sequence, Tuple


@dataclass(frozen=True)
class TestCase:
    id: str
    tier: int
    area: str
    features: Tuple[str, ...]
    title: str
    baseline: bool
    fn: Callable[[], None]


@dataclass
class TestResult:
    id: str
    tier: int
    area: str
    features: List[str]
    title: str
    baseline: bool
    status: str  # "PASS" | "FAIL"
    duration_s: float
    reason: str


_REGISTRY: List[TestCase] = []
_REGISTERED_IDS: set[str] = set()


class _TestTimeout(AssertionError):
    pass


def e2e_test(
    *,
    id: str,
    tier: int,
    area: str,
    features: Sequence[str],
    title: str,
    baseline: bool = False,
) -> Callable[[Callable[[], None]], Callable[[], None]]:
    """Decorator registering an opaque-box E2E test."""
    if tier not in (1, 2, 3, 4):
        raise ValueError(f"Invalid tier {tier!r} for test {id}")

    def decorator(fn: Callable[[], None]) -> Callable[[], None]:
        if id in _REGISTERED_IDS:
            raise ValueError(f"Duplicate E2E test id: {id}")
        _REGISTERED_IDS.add(id)
        _REGISTRY.append(
            TestCase(
                id=id,
                tier=tier,
                area=area,
                features=tuple(features),
                title=title,
                baseline=baseline,
                fn=fn,
            )
        )
        return fn

    return decorator


def registered_tests() -> List[TestCase]:
    return sorted(_REGISTRY, key=lambda t: (t.tier, t.id))


def select_tests(
    *,
    tier: str,
    baseline_only: bool = False,
    filter_regex: Optional[str] = None,
) -> List[TestCase]:
    tests = registered_tests()
    if tier != "all":
        tier_num = int(tier)
        tests = [t for t in tests if t.tier == tier_num]
    if baseline_only:
        tests = [t for t in tests if t.baseline]
    if filter_regex:
        pat = re.compile(filter_regex, re.IGNORECASE)
        tests = [
            t
            for t in tests
            if pat.search(t.id)
            or pat.search(t.title)
            or pat.search(t.area)
            or any(pat.search(f) for f in t.features)
        ]
    return tests


def _clean_reason(exc: BaseException) -> str:
    msg = str(exc).strip()
    if not msg:
        msg = exc.__class__.__name__
    msg = re.sub(r"\s+", " ", msg)
    return msg[:260]


def _run_one(tc: TestCase, timeout_sec: float) -> TestResult:
    start = time.monotonic()

    def _alarm_handler(signum, frame):
        raise _TestTimeout(f"Test exceeded wall-clock timeout ({timeout_sec:.0f}s)")

    prev_handler = signal.signal(signal.SIGALRM, _alarm_handler)
    signal.setitimer(signal.ITIMER_REAL, timeout_sec)
    try:
        tc.fn()
        status = "PASS"
        reason = ""
    except AssertionError as exc:
        status = "FAIL"
        reason = _clean_reason(exc)
    except Exception as exc:
        status = "FAIL"
        tb_last = traceback.format_exc().strip().splitlines()[-1]
        reason = f"{exc.__class__.__name__}: {_clean_reason(exc)} ({tb_last[:120]})"[:260]
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, prev_handler)

    duration = time.monotonic() - start
    return TestResult(
        id=tc.id,
        tier=tc.tier,
        area=tc.area,
        features=list(tc.features),
        title=tc.title,
        baseline=tc.baseline,
        status=status,
        duration_s=round(duration, 3),
        reason=reason,
    )


def format_summary_table(results: List[TestResult], total_duration_s: float) -> str:
    header = (
        f"{'Tier':<5} | {'Test ID':<10} | {'Features':<18} | "
        f"{'Status':<6} | {'Duration':>8} | Reason"
    )
    sep = "-" * 112
    lines = [header, sep]
    for r in results:
        feats = ",".join(r.features)
        if len(feats) > 18:
            feats = feats[:15] + "..."
        lines.append(
            f"T{r.tier:<4} | {r.id:<10} | {feats:<18} | "
            f"{r.status:<6} | {r.duration_s:>7.2f}s | {r.reason}"
        )
    passed = sum(1 for r in results if r.status == "PASS")
    failed = sum(1 for r in results if r.status != "PASS")
    lines.append(sep)
    lines.append(
        f"Total: {len(results)} | Passed: {passed} | Failed: {failed} | Duration: {total_duration_s:.2f}s"
    )
    return "\n".join(lines)


def execute_suite(
    *,
    tier: str,
    baseline_only: bool = False,
    filter_regex: Optional[str] = None,
    fail_fast: bool = False,
    list_only: bool = False,
    json_report: Optional[Path] = None,
) -> int:
    try:
        selected = select_tests(
            tier=tier,
            baseline_only=baseline_only,
            filter_regex=filter_regex,
        )
    except re.error as exc:
        print(
            f"ERROR: invalid --filter regex {filter_regex!r}: {exc}",
            file=sys.stderr,
        )
        return 2
    if not selected:
        print(
            f"ERROR: 0 tests matched (tier={tier!r}, baseline_only={baseline_only}, filter={filter_regex!r})",
            file=sys.stderr,
        )
        return 2

    if list_only:
        print(
            f"{'Tier':<5} | {'Test ID':<10} | {'Base':<4} | {'Features':<22} | Title"
        )
        print("-" * 100)
        for tc in selected:
            feats = ",".join(tc.features)
            if len(feats) > 22:
                feats = feats[:19] + "..."
            base = "yes" if tc.baseline else "no"
            print(f"T{tc.tier:<4} | {tc.id:<10} | {base:<4} | {feats:<22} | {tc.title}")
        print("-" * 100)
        print(f"Listed {len(selected)} test(s).")
        return 0

    timeout_sec = float(os.environ.get("E2E_TEST_TIMEOUT_SEC", "120"))
    t0 = time.monotonic()
    results: List[TestResult] = []
    for tc in selected:
        res = _run_one(tc, timeout_sec=timeout_sec)
        results.append(res)
        if fail_fast and res.status != "PASS":
            break

    total_duration = time.monotonic() - t0
    print(format_summary_table(results, total_duration))

    passed = sum(1 for r in results if r.status == "PASS")
    failed = sum(1 for r in results if r.status != "PASS")

    if json_report is not None:
        json_report.parent.mkdir(parents=True, exist_ok=True)
        payload: Dict[str, object] = {
            "tier": tier,
            "baseline_only": baseline_only,
            "filter": filter_regex,
            "total": len(results),
            "passed": passed,
            "failed": failed,
            "duration_s": round(total_duration, 3),
            "results": [asdict(r) for r in results],
        }
        json_report.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

    return 0 if (failed == 0 and len(results) > 0) else 1
