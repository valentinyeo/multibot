#!/usr/bin/env python3
"""CLI entrypoint for the Multibot opaque-box 4-tier E2E test suite."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.dont_write_bytecode = True

# Ensure repo root is on sys.path so `import e2e...` works from any cwd
REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

# Register all 4 tiers
import e2e.tier1  # noqa: F401, E402
import e2e.tier2  # noqa: F401, E402
import e2e.tier3  # noqa: F401, E402
import e2e.tier4  # noqa: F401, E402
from e2e.lib.runner import execute_suite  # noqa: E402


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="e2e/run_e2e.sh",
        description="Multibot opaque-box 4-tier E2E test runner",
    )
    parser.add_argument(
        "--tier",
        required=True,
        choices=("1", "2", "3", "4", "all"),
        help="Required test tier to execute (1, 2, 3, 4, or all)",
    )
    parser.add_argument(
        "--baseline-only",
        action="store_true",
        help="Run only baseline Slice 1 + Hard Invariants tests",
    )
    parser.add_argument(
        "--filter",
        dest="filter_regex",
        default=None,
        help="Optional case-insensitive regex matching Test ID, title, area, or feature",
    )
    parser.add_argument(
        "--json-report",
        type=Path,
        default=None,
        help="Write structured JSON test report to this path",
    )
    parser.add_argument(
        "--fail-fast",
        action="store_true",
        help="Stop execution after the first failing test",
    )
    parser.add_argument(
        "--list",
        dest="list_only",
        action="store_true",
        help="List matching tests without executing them",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    try:
        args = parse_args(argv)
    except SystemExit as exc:
        code = exc.code if isinstance(exc.code, int) else 2
        return 0 if code == 0 else 2

    return execute_suite(
        tier=args.tier,
        baseline_only=args.baseline_only,
        filter_regex=args.filter_regex,
        fail_fast=args.fail_fast,
        list_only=args.list_only,
        json_report=args.json_report,
    )


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
