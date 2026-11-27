#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

usage() {
  cat >&2 <<'EOF'
Usage: ./e2e/run_e2e.sh --tier {1|2|3|4|all} [OPTIONS]

Required:
  --tier {1|2|3|4|all}     Select test tier to run (1, 2, 3, 4, or all)

Options:
  --baseline-only          Run only baseline Slice 1 + Hard Invariants tests
  --filter <regex>         Filter tests by regex (ID, title, area, or feature)
  --isolated-target        Use isolated CARGO_TARGET_DIR under TMPDIR
  --no-build               Skip `cargo build --release -p multibotd` pre-step
  --json-report <path>     Write structured JSON report to <path>
  --fail-fast              Stop after first failing test
  --list                   List matching tests without running them
  -h, --help               Show this help text

Exit codes:
  0  All executed tests passed (total > 0) or --list/--help succeeded
  1  One or more executed tests failed
  2  Invalid CLI arguments or 0 tests matched filter
EOF
}

if [[ $# -eq 0 ]]; then
  usage
  exit 2
fi

TIER=""
BASELINE_ONLY=0
FILTER_REGEX=""
ISOLATED_TARGET=0
NO_BUILD=0
JSON_REPORT=""
FAIL_FAST=0
LIST_ONLY=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tier)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --tier requires an argument (1|2|3|4|all)" >&2
        usage
        exit 2
      fi
      TIER="$2"
      shift 2
      ;;
    --tier=*)
      TIER="${1#*=}"
      shift
      ;;
    --baseline-only)
      BASELINE_ONLY=1
      shift
      ;;
    --filter)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --filter requires a regex argument" >&2
        exit 2
      fi
      FILTER_REGEX="$2"
      shift 2
      ;;
    --filter=*)
      FILTER_REGEX="${1#*=}"
      shift
      ;;
    --isolated-target)
      ISOLATED_TARGET=1
      shift
      ;;
    --no-build)
      NO_BUILD=1
      shift
      ;;
    --json-report)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --json-report requires a path argument" >&2
        exit 2
      fi
      JSON_REPORT="$2"
      shift 2
      ;;
    --json-report=*)
      JSON_REPORT="${1#*=}"
      shift
      ;;
    --fail-fast)
      FAIL_FAST=1
      shift
      ;;
    --list)
      LIST_ONLY=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "ERROR: Unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

case "${TIER}" in
  1|2|3|4|all) ;;
  *)
    echo "ERROR: --tier must be one of {1|2|3|4|all}, got '${TIER}'" >&2
    usage
    exit 2
    ;;
esac

if [[ "${ISOLATED_TARGET}" -eq 1 ]]; then
  export CARGO_TARGET_DIR="${E2E_CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/multibot-e2e-target}"
fi

TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"

if [[ "${LIST_ONLY}" -eq 0 && "${NO_BUILD}" -eq 0 ]]; then
  if ! cargo build --release -p multibotd --manifest-path "${REPO_ROOT}/Cargo.toml" >/dev/null 2>&1; then
    if [[ -x "${TARGET_DIR}/release/multibotd" || -x "${REPO_ROOT}/target/release/multibotd" ]]; then
      echo "WARN: cargo build --release -p multibotd failed (workspace may be mid-edit); falling back to existing multibotd binary" >&2
    else
      echo "ERROR: cargo build --release -p multibotd failed and no prebuilt multibotd binary exists" >&2
      exit 1
    fi
  fi
fi

if [[ -z "${E2E_MULTIBOTD_BIN:-}" ]]; then
  if [[ -x "${TARGET_DIR}/release/multibotd" ]]; then
    export E2E_MULTIBOTD_BIN="${TARGET_DIR}/release/multibotd"
  elif [[ -x "${REPO_ROOT}/target/release/multibotd" ]]; then
    export E2E_MULTIBOTD_BIN="${REPO_ROOT}/target/release/multibotd"
  fi
fi

PY_ARGS=(--tier "${TIER}")
[[ "${BASELINE_ONLY}" -eq 1 ]] && PY_ARGS+=(--baseline-only)
[[ -n "${FILTER_REGEX}" ]] && PY_ARGS+=(--filter "${FILTER_REGEX}")
[[ -n "${JSON_REPORT}" ]] && PY_ARGS+=(--json-report "${JSON_REPORT}")
[[ "${FAIL_FAST}" -eq 1 ]] && PY_ARGS+=(--fail-fast)
[[ "${LIST_ONLY}" -eq 1 ]] && PY_ARGS+=(--list)

export PYTHONDONTWRITEBYTECODE=1
exec python3 -B "${SCRIPT_DIR}/run_suite.py" "${PY_ARGS[@]}"
