"""Tier 1 Priority 1 Auto-Updater & GitHub Releases CI tests (T1-UPD-01..07)."""

from __future__ import annotations

import hashlib
import json
import re
import tomllib
import urllib.request
import zipfile
from io import BytesIO
from pathlib import Path

from e2e.lib import REPO_ROOT
from e2e.lib.cargo_runner import CARGO
from e2e.lib.daemon_driver import DaemonDriver, probe_cli_version
from e2e.lib.mock_release_server import MockReleaseServer, MockReleaseSpec
from e2e.lib.runner import e2e_test
from e2e.lib.sandbox import IsolatedHome
from e2e.lib.static_checks import (
    production_rust_source,
    read_crate_production_sources,
    scan_update_copy_violations,
)
from e2e.lib.yaml_util import load_yaml


def _needs_list(job_cfg: dict) -> list[str]:
    needs = job_cfg.get("needs", [])
    if isinstance(needs, str):
        return [needs]
    if isinstance(needs, list):
        return [str(x) for x in needs]
    return []


def _collect_strings(obj: object) -> str:
    """Recursively collect unescaped string representations from parsed YAML nodes."""
    parts: list[str] = []

    def _walk(node: object) -> None:
        if isinstance(node, str):
            parts.append(node)
        elif isinstance(node, dict):
            for k, v in node.items():
                parts.append(str(k))
                _walk(v)
        elif isinstance(node, list):
            for item in node:
                _walk(item)
        elif node is not None:
            parts.append(str(node))

    _walk(obj)
    return "\n".join(parts)


def _strip_hash_comments(text: str) -> str:
    """Strip both whole-line and trailing inline `# ...` comments outside single/double quotes."""
    cleaned_lines: list[str] = []
    for ln in text.splitlines():
        buf: list[str] = []
        in_single = False
        in_double = False
        i = 0
        n = len(ln)
        while i < n:
            ch = ln[i]
            if in_double:
                buf.append(ch)
                if ch == "\\" and i + 1 < n:
                    buf.append(ln[i + 1])
                    i += 2
                    continue
                if ch == '"':
                    in_double = False
                i += 1
                continue
            if in_single:
                buf.append(ch)
                if ch == "'":
                    in_single = False
                i += 1
                continue
            if ch == "#":
                break
            if ch == '"':
                in_double = True
            elif ch == "'":
                prev_is_alnum = i > 0 and ln[i - 1].isalnum()
                has_closing_quote = ln.find("'", i + 1) != -1
                if not prev_is_alnum and has_closing_quote:
                    in_single = True
            buf.append(ch)
            i += 1
        stripped = "".join(buf)
        if stripped.strip():
            cleaned_lines.append(stripped)
    return "\n".join(cleaned_lines)


def _strip_outer_gh_expr(expr: str) -> str:
    """Strip optional outer `${{ ... }}` wrapper if it spans the entire expression."""
    s = expr.strip()
    if s.startswith("${{") and s.endswith("}}") and s.find("}}", 3) == len(s) - 2:
        return s[3:-2].strip()
    return s


def _strip_matching_outer_parens(expr: str) -> str:
    """Iteratively unwrap matching outer parentheses `( ... )`, respecting quotes."""
    s = expr.strip()
    while s.startswith("(") and s.endswith(")"):
        depth = 0
        in_single = False
        in_double = False
        matched_end = -1
        i = 0
        n = len(s)
        while i < n:
            ch = s[i]
            if in_single:
                if ch == "'":
                    if i + 1 < n and s[i + 1] == "'":
                        i += 2
                        continue
                    in_single = False
            elif in_double:
                if ch == "\\" and i + 1 < n:
                    i += 2
                    continue
                if ch == '"':
                    in_double = False
            else:
                if ch == "'":
                    in_single = True
                elif ch == '"':
                    in_double = True
                elif ch == "(":
                    depth += 1
                elif ch == ")":
                    depth -= 1
                    if depth == 0:
                        matched_end = i
                        break
            i += 1
        if matched_end == n - 1:
            s = s[1:-1].strip()
        else:
            break
    return s


def _split_top_level_op(expr: str, op: str) -> list[str]:
    """Split `expr` on top-level binary operator `op` (`||` or `&&`) outside quotes and parens."""
    parts: list[str] = []
    depth = 0
    in_single = False
    in_double = False
    start = 0
    i = 0
    n = len(expr)
    op_len = len(op)
    while i < n:
        ch = expr[i]
        if in_single:
            if ch == "'":
                if i + 1 < n and expr[i + 1] == "'":
                    i += 2
                    continue
                in_single = False
            i += 1
            continue
        if in_double:
            if ch == "\\" and i + 1 < n:
                i += 2
                continue
            if ch == '"':
                in_double = False
            i += 1
            continue
        if ch == "'":
            in_single = True
        elif ch == '"':
            in_double = True
        elif ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth < 0:
                raise AssertionError(f"Unbalanced parentheses in release `if:` expression: {expr!r}")
        elif depth == 0 and expr[i : i + op_len] == op:
            parts.append(expr[start:i].strip())
            i += op_len
            start = i
            continue
        i += 1
    if depth != 0 or in_single or in_double:
        raise AssertionError(
            f"Unbalanced quotes or parentheses in release `if:` expression: {expr!r}"
        )
    parts.append(expr[start:].strip())
    return parts


def _needs_closure(jobs: dict, root_job: str) -> set[str]:
    """Compute transitive `needs` closure starting at `root_job` (excludes un-chained jobs)."""
    visited: set[str] = set()
    stack = [root_job]
    while stack:
        cur = stack.pop()
        if cur in visited or cur not in jobs or not isinstance(jobs[cur], dict):
            continue
        visited.add(cur)
        for dep in _needs_list(jobs[cur]):
            if dep not in visited:
                stack.append(dep)
    return visited


_DIRECT_PUSH_GUARD_RE = re.compile(
    r"^github\.event_name\s*==\s*(['\"])push\1$"
    r"|^github\.event_name\s*!=\s*(['\"])pull_request\2$"
)
_UPSTREAM_GATE_RE = re.compile(
    r"^needs\.([A-Za-z0-9_-]+)\.outputs\.([A-Za-z0-9_-]+)\s*==\s*(['\"])true\3$"
)
_STEP_OUTPUT_EXPR_RE = re.compile(
    r"^steps\.([A-Za-z0-9_-]+)\.outputs\.([A-Za-z0-9_-]+)$"
)
_NEEDS_OUTPUT_EXPR_RE = re.compile(
    r"^needs\.([A-Za-z0-9_-]+)\.outputs\.([A-Za-z0-9_-]+)$"
)


def _validate_upstream_output_gate(
    up_job: str,
    up_out: str,
    allowed_needs: set[str],
    jobs: dict,
    reasons: list[str],
    visited: set[tuple[str, str]] | None = None,
) -> bool:
    """Validate that `needs.<up_job>.outputs.<up_out> == 'true'` is backed by a real non-PR upstream gate."""
    seen = set() if visited is None else set(visited)
    key = (up_job, up_out)
    if key in seen:
        reasons.append(f"cyclic upstream output reference detected at {up_job}.outputs.{up_out}")
        return False
    seen.add(key)

    if up_job not in allowed_needs:
        reasons.append(
            f"upstream job {up_job!r} is not declared in allowed needs ({sorted(allowed_needs)})"
        )
        return False
    up_cfg = jobs.get(up_job)
    if not isinstance(up_cfg, dict):
        reasons.append(f"upstream job {up_job!r} does not exist in workflow jobs")
        return False
    up_outputs = up_cfg.get("outputs")
    if not isinstance(up_outputs, dict) or up_out not in up_outputs:
        reasons.append(f"upstream job {up_job!r} does not declare output {up_out!r}")
        return False

    raw_out_expr = str(up_outputs.get(up_out, "")).strip()
    is_gh_wrapped = (
        raw_out_expr.startswith("${{")
        and raw_out_expr.endswith("}}")
        and raw_out_expr.find("}}", 3) == len(raw_out_expr) - 2
    )
    if not is_gh_wrapped:
        reasons.append(
            f"upstream job {up_job!r} output {up_out!r} must be wrapped in a single `${{{{ ... }}}}` "
            f"expression (no unwrapped literals), got {raw_out_expr!r}"
        )
        return False

    inner = raw_out_expr[3:-2].strip()
    needs_m = _NEEDS_OUTPUT_EXPR_RE.fullmatch(inner)
    if needs_m:
        dep_job, dep_out = needs_m.group(1), needs_m.group(2)
        return _validate_upstream_output_gate(
            dep_job,
            dep_out,
            set(_needs_list(up_cfg)),
            jobs,
            reasons,
            visited=seen,
        )

    step_m = _STEP_OUTPUT_EXPR_RE.fullmatch(inner)
    if not step_m:
        reasons.append(
            f"upstream job {up_job!r} output {up_out!r} must be a direct `steps.<id>.outputs.<name>` "
            f"or `needs.<job>.outputs.<name>` expression (no operators or literals), got {raw_out_expr!r}"
        )
        return False

    step_id, step_out = step_m.group(1), step_m.group(2)
    raw_steps = up_cfg.get("steps", [])
    matching_steps = [
        s
        for s in (raw_steps if isinstance(raw_steps, list) else [])
        if isinstance(s, dict) and s.get("id") is not None and str(s.get("id")) == step_id
    ]
    if not matching_steps:
        reasons.append(
            f"upstream job {up_job!r} output {up_out!r} references nonexistent step id {step_id!r}"
        )
        return False
    if len(matching_steps) > 1:
        reasons.append(
            f"upstream job {up_job!r} output {up_out!r} references ambiguous duplicate step id {step_id!r}"
        )
        return False
    target_step = matching_steps[0]

    step_text = _strip_hash_comments(_collect_strings(target_step))
    up_text = _strip_hash_comments(
        _collect_strings([up_cfg.get("if"), up_cfg.get("env"), target_step])
    )
    has_event_check = ("github.event_name" in up_text) and (
        "push" in up_text or "pull_request" in up_text
    )
    has_ref_or_tag_check = any(
        tok in up_text
        for tok in (
            "refs/heads/main",
            "github.ref",
            "refs/tags/v",
            'startswith("v")',
            "startswith('v')",
            "v*",
        )
    )
    if not (has_event_check and has_ref_or_tag_check):
        reasons.append(
            f"upstream job {up_job!r} step {step_id!r} must inspect github.event_name and push/main/tag ref conditions"
        )
        return False

    has_false_assign = bool(
        re.search(rf"\b{re.escape(step_out)}\s*=\s*['\"]?false['\"]?", step_text, re.IGNORECASE)
    )
    pr_exclude_re = re.compile(r"!=\s*(['\"])pull_request\1")
    if not (has_false_assign or pr_exclude_re.search(up_text)):
        reasons.append(
            f"upstream job {up_job!r} step {step_id!r} must explicitly set/default {step_out!r} to false "
            f"for non-release/PR paths or exclude pull_request"
        )
        return False
    return True


def _expr_enforces_non_pr_guard(
    expr: str,
    rel_needs: set[str],
    jobs: dict,
    reasons: list[str],
) -> bool:
    """Return True iff every top-level `||` branch has at least one mandatory non-negated non-PR guard."""
    s = _strip_matching_outer_parens(_strip_outer_gh_expr(expr))
    if not s:
        reasons.append("empty `if:` branch")
        return False

    or_branches = _split_top_level_op(s, "||")
    if len(or_branches) > 1:
        for branch in or_branches:
            if not _expr_enforces_non_pr_guard(branch, rel_needs, jobs, reasons):
                reasons.append(f"`||` branch {branch!r} does not enforce non-PR guard")
                return False
        return True

    and_conjuncts = _split_top_level_op(s, "&&")
    if len(and_conjuncts) > 1:
        branch_reasons: list[str] = []
        for conj in and_conjuncts:
            if _expr_enforces_non_pr_guard(conj, rel_needs, jobs, branch_reasons):
                return True
        reasons.extend(branch_reasons)
        return False

    if s.startswith("!"):
        reasons.append(f"negated guard is forbidden: {s!r}")
        return False

    if _DIRECT_PUSH_GUARD_RE.match(s):
        return True

    up_m = _UPSTREAM_GATE_RE.match(s)
    if up_m:
        return _validate_upstream_output_gate(
            up_m.group(1), up_m.group(2), rel_needs, jobs, reasons
        )

    reasons.append(f"unrecognized/non-PR-safe guard conjunct: {s!r}")
    return False


def _validate_release_if_gate(rel_job: dict, jobs: dict) -> None:
    """Verify jobs.release.if excludes PRs across every `||` branch via direct or verified upstream guard."""
    rel_if = str(rel_job.get("if", "")).strip()
    assert rel_if, "release job must define an `if:` condition excluding PRs"

    rel_needs = set(_needs_list(rel_job))
    reasons: list[str] = []
    if not _expr_enforces_non_pr_guard(rel_if, rel_needs, jobs, reasons):
        detail = "; ".join(dict.fromkeys(reasons)) if reasons else "no valid non-PR guard"
        raise AssertionError(
            f"release job `if:` gate {rel_if!r} failed non-PR guard validation: {detail}"
        )


def _validate_release_asset_pipeline(jobs: dict) -> None:
    """Verify jobs.release publishes the 3 release assets and its needs closure builds/packages binaries & manifest."""
    assert "release" in jobs and isinstance(jobs["release"], dict), "ci.yml missing `release` job"
    rel_text = _strip_hash_comments(_collect_strings(jobs["release"]))
    assert re.search(
        r"gh\s+release\s+(?:create|upload)|action-gh-release|release-action"
        r"|api\.github\.com/repos/[^\s]+/releases|gh\s+api\s+[^\n]*/releases",
        rel_text,
    ), (
        "ci.yml release job must invoke a real GitHub Release publication command/action "
        "(e.g., `gh release create`/`upload`, `action-gh-release`, `release-action`, or GitHub Releases API)"
    )
    for published_asset in (
        "multibot-windows-x64.zip",
        "SHA256SUMS.txt",
        "update-manifest.json",
    ):
        assert published_asset in rel_text, (
            f"ci.yml release job missing required published release asset: {published_asset}"
        )

    closure_names = _needs_closure(jobs, "release")
    pipeline_text = _strip_hash_comments(
        "\n".join(_collect_strings(jobs[name]) for name in sorted(closure_names))
    )
    for required_field in (
        "multibot.exe",
        "multibotd-linux-x86_64",
        "schema_version",
    ):
        assert required_field in pipeline_text, (
            f"ci.yml release pipeline closure ({sorted(closure_names)}) missing required "
            f"asset/schema field: {required_field}"
        )


@e2e_test(
    id="T1-UPD-01",
    tier=1,
    area="updater_ci",
    features=("F3",),
    title="CI parallel linux+windows jobs, gated release job & MULTIBOT_BUILD_VERSION",
    baseline=False,
)
def test_ci_parallel_jobs_and_gated_release() -> None:
    ci_path = REPO_ROOT / ".github" / "workflows" / "ci.yml"
    wf = load_yaml(ci_path)
    raw_ci = ci_path.read_text(encoding="utf-8")

    # YAML 1.1 parsers (Ruby JSON.generate) turn `on:` into string `"true"`, PyYAML into `True`
    triggers = wf.get("on") or wf.get("true") or wf.get(True) or {}
    push_cfg = triggers.get("push", {}) if isinstance(triggers, dict) else {}
    tags = push_cfg.get("tags", []) if isinstance(push_cfg, dict) else []
    assert any("v*" in str(t) for t in tags), (
        f"ci.yml push triggers must include tags matching 'v*', got {push_cfg!r}"
    )

    jobs = wf.get("jobs", {})
    assert "linux" in jobs and "windows" in jobs, "ci.yml must define linux and windows jobs"
    win_needs = _needs_list(jobs["windows"])
    assert "linux" not in win_needs, (
        f"windows job must run in PARALLEL with linux (found needs={win_needs!r})"
    )

    assert "release" in jobs, "ci.yml missing gated `release` job"
    rel_job = jobs["release"]
    rel_needs = set(_needs_list(rel_job))
    assert {"linux", "windows"}.issubset(rel_needs), (
        f"release job must depend on both linux and windows, got needs={sorted(rel_needs)}"
    )

    rel_perms = rel_job.get("permissions") or wf.get("permissions") or {}
    assert isinstance(rel_perms, dict) and rel_perms.get("contents") == "write", (
        f"release job requires permissions.contents == 'write', got {rel_perms!r}"
    )

    _validate_release_if_gate(rel_job, jobs)

    assert "MULTIBOT_BUILD_VERSION" in raw_ci, (
        "ci.yml must compute and inject MULTIBOT_BUILD_VERSION into linux and windows builds"
    )


@e2e_test(
    id="T1-UPD-02",
    tier=1,
    area="updater_ci",
    features=("F3",),
    title="CI release asset contract (zip + SHA256SUMS.txt + update-manifest.json)",
    baseline=False,
)
def test_ci_release_asset_contract_and_bundle_validation() -> None:
    ci_path = REPO_ROOT / ".github" / "workflows" / "ci.yml"
    wf = load_yaml(ci_path)
    jobs = wf.get("jobs", {})
    _validate_release_asset_pipeline(jobs)

    # Also validate a real packaged bundle served over HTTP using actual target/release/multibotd
    with MockReleaseServer([MockReleaseSpec(version="0.1.99")]) as srv:
        with urllib.request.urlopen(srv.latest_url, timeout=3.0) as resp:
            assert resp.status == 200
            rel_json = json.loads(resp.read().decode("utf-8"))
        asset_urls = {a["name"]: a["browser_download_url"] for a in rel_json["assets"]}
        assert {
            "multibot-windows-x64.zip",
            "SHA256SUMS.txt",
            "update-manifest.json",
        }.issubset(asset_urls.keys())

        manifest = json.loads(
            urllib.request.urlopen(asset_urls["update-manifest.json"], timeout=3.0).read()
        )
        zip_data = urllib.request.urlopen(
            asset_urls["multibot-windows-x64.zip"], timeout=3.0
        ).read()
        assert manifest["schema_version"] == 1
        win = manifest["windows_x64"]
        assert hashlib.sha256(zip_data).hexdigest() == win["sha256"]
        with zipfile.ZipFile(BytesIO(zip_data), "r") as zf:
            assert set(zf.namelist()) == {"multibot.exe", "multibotd-linux-x86_64"}
            assert hashlib.sha256(zf.read("multibot.exe")).hexdigest() == win["multibot_exe_sha256"]
            assert (
                hashlib.sha256(zf.read("multibotd-linux-x86_64")).hexdigest()
                == win["multibotd_linux_x86_64_sha256"]
            )


@e2e_test(
    id="T1-UPD-03",
    tier=1,
    area="updater_ci",
    features=("F1", "F4"),
    title="Build-time version & binary_hash embedding (multibot-proto + multibotd)",
    baseline=False,
)
def test_build_version_and_binary_hash_embedding() -> None:
    proto_rs = (REPO_ROOT / "crates" / "multibot-proto" / "src" / "lib.rs").read_text(
        encoding="utf-8"
    )
    assert "pub fn build_version" in proto_rs and "MULTIBOT_BUILD_VERSION" in proto_rs, (
        "multibot-proto must export pub fn build_version() reading MULTIBOT_BUILD_VERSION"
    )

    rc, out, err = probe_cli_version(timeout=2.0)
    assert rc == 0 and re.search(r"\d+\.\d+\.\d+", out), (
        f"multibotd --version must exit 0 and print SemVer version; got rc={rc}, stdout={out!r}, stderr={err!r}"
    )

    with IsolatedHome(prefix="mb_t1_upd03_") as sb:
        with DaemonDriver(sb) as dmn:
            ack = dmn.request({"op": "hello", "client_version": "0.1.0"}, "hello_ack")
            assert re.match(r"^\d+\.\d+\.\d+", str(ack.get("daemon_version", ""))), (
                f"HelloAck daemon_version must be SemVer, got {ack!r}"
            )
            bhash = str(ack.get("binary_hash", ""))
            assert re.match(r"^[0-9a-fA-F]{12,}$", bhash), (
                f"HelloAck must include non-empty >=12-hex binary_hash, got {ack!r}"
            )


@e2e_test(
    id="T1-UPD-04",
    tier=1,
    area="updater_ci",
    features=("F5", "F6"),
    title="Background release poller ETag / 304 Not Modified & MULTIBOT_UPDATE_URL",
    baseline=False,
)
def test_updater_poller_etag_and_304() -> None:
    updater_toml = REPO_ROOT / "crates" / "multibot-updater" / "Cargo.toml"
    assert updater_toml.is_file(), "crates/multibot-updater/Cargo.toml not found (expected M1/M2)"

    root_toml = tomllib.loads((REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    members = root_toml.get("workspace", {}).get("members", [])
    assert "crates/multibot-updater" in members, (
        "crates/multibot-updater must be a workspace member in root Cargo.toml"
    )

    prod_src = read_crate_production_sources(REPO_ROOT / "crates" / "multibot-updater" / "src")
    assert "MULTIBOT_UPDATE_URL" in prod_src, (
        "crates/multibot-updater production code missing MULTIBOT_UPDATE_URL override"
    )
    assert "if-none-match" in prod_src.lower(), (
        "crates/multibot-updater production code missing If-None-Match ETag header"
    )
    assert re.search(r"\b304\b|\bNOT_MODIFIED\b", prod_src), (
        "crates/multibot-updater production code must handle HTTP 304 / StatusCode::NOT_MODIFIED"
    )

    CARGO.assert_crate_tests_pass(
        "multibot-updater",
        min_expected=8,
        required_any_of=[
            ("etag", "304", "not_modified", "if_none_match", "poll", "check_update", "latest"),
        ],
    )


@e2e_test(
    id="T1-UPD-05",
    tier=1,
    area="updater_ci",
    features=("F7", "F8"),
    title="Secure download, SHA-256 verification & Windows atomic swap traits",
    baseline=False,
)
def test_updater_download_verify_and_atomic_swap() -> None:
    prod_src = read_crate_production_sources(REPO_ROOT / "crates" / "multibot-updater" / "src")
    for token in ("UpdateState", "rejected.txt", "--update-ready", "multibot.exe"):
        assert token in prod_src, (
            f"crates/multibot-updater production code missing required contract token: {token}"
        )
    assert any(t in prod_src for t in ("SHA256SUMS", "multibot_exe_sha256", "update-manifest.json")), (
        "crates/multibot-updater must verify SHA-256 via SHA256SUMS, multibot_exe_sha256, or update-manifest.json"
    )
    assert all(t in prod_src for t in ("FileSwapper", "UpdaterPlatform", "ProcessLauncher")), (
        "crates/multibot-updater must expose FileSwapper, ProcessLauncher, and UpdaterPlatform traits"
    )
    has_cli_arg_forwarding = (
        "--gpu" in prod_src
        or (
            "env::args" in prod_src
            and "preserved_cli_args" in prod_src
            and "spawn_relaunch" in prod_src
        )
    )
    assert has_cli_arg_forwarding, (
        "crates/multibot-updater production code must preserve CLI flags (including --gpu) across relaunch "
        "via explicit --gpu handling or std::env::args() -> preserved_cli_args() -> spawn_relaunch()"
    )

    CARGO.assert_crate_tests_pass(
        "multibot-updater",
        min_expected=8,
        required_any_of=[
            ("swap", "apply", "stage", "install", "relaunch"),
            ("sha256", "checksum", "verify", "digest", "hash", "tamper", "download"),
        ],
    )


@e2e_test(
    id="T1-UPD-06",
    tier=1,
    area="updater_ci",
    features=("F10",),
    title="Remote multibotd version+SHA-256 atomic deploy (daemon_deploy.rs)",
    baseline=False,
)
def test_remote_daemon_deploy_version_and_sha12() -> None:
    deploy_rs = REPO_ROOT / "crates" / "multibot-core" / "src" / "daemon_deploy.rs"
    assert deploy_rs.is_file(), "crates/multibot-core/src/daemon_deploy.rs not found (expected M2)"
    src = production_rust_source(deploy_rs.read_text(encoding="utf-8"))
    for token in (".multibot", "multibotd-", ".tmp", "mv -f", "ln -sf"):
        assert token in src, f"daemon_deploy.rs missing atomic remote deploy token: {token}"
    assert "sha256" in src.lower() or "Sha256" in src, (
        "daemon_deploy.rs must compute 12-hex SHA-256 prefix of bundled multibotd-linux-x86_64"
    )

    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="daemon_deploy", min_expected=2
    )


@e2e_test(
    id="T1-UPD-07",
    tier=1,
    area="updater_ci",
    features=("F9",),
    title="Sleek sidebar Update Card, Ctrl+K Update commands & settings persistence",
    baseline=False,
)
def test_sidebar_update_card_and_ctrl_k_commands() -> None:
    update_ui = REPO_ROOT / "crates" / "multibot-desktop" / "src" / "update_ui.rs"
    assert update_ui.is_file(), "crates/multibot-desktop/src/update_ui.rs not found"
    ui_src = production_rust_source(update_ui.read_text(encoding="utf-8"))

    for copy_tok in ("is ready", "Update & Restart", "Later"):
        assert copy_tok in ui_src, (
            f"update_ui.rs missing required Update Card UI copy: {copy_tok!r}"
        )

    desktop_src = read_crate_production_sources(REPO_ROOT / "crates" / "multibot-desktop" / "src")
    for cmd_label in (
        "Update: Check for Updates",
        "Update: Download & Relaunch",
        "Restart Multi Bot",
        "Changelog",
    ):
        assert cmd_label in desktop_src, (
            f"Desktop missing required Ctrl+K update command: {cmd_label!r}"
        )

    settings_rs = production_rust_source(
        (REPO_ROOT / "crates" / "multibot-core" / "src" / "settings.rs").read_text(
            encoding="utf-8"
        )
    )
    for field in ("dismissed_update_version", "last_update_etag", "auto_check_updates"):
        assert field in settings_rs, (
            f"MultibotSettings missing required updater field: {field}"
        )

    violations = scan_update_copy_violations(REPO_ROOT)
    assert not violations, f"Update card violated zero-duration copy rule: {violations}"

    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="update", min_expected=1)
