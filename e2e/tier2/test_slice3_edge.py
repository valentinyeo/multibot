"""Tier 2 Slice 3 boundary & corner-case tests (T2-S3-01..05)."""

from __future__ import annotations

import re

from e2e.lib import REPO_ROOT
from e2e.lib.cargo_runner import CARGO
from e2e.lib.runner import e2e_test
from e2e.lib.static_checks import (
    extract_rust_brace_block,
    find_ctrl_alt_bindings,
    mask_rust_string_and_char_literals,
    production_rust_source,
    read_crate_production_sources,
)

_FRECENCY_CAP_15_RE = re.compile(
    r"(?:min|clamp|cap|max_freq|freq|frecency)[^\n;]{0,60}\b15(?:\.0|_?(?:usize|u32|i32|f32|f64))?\b"
    r"|\b15(?:\.0|_?(?:usize|u32|i32|f32|f64))?\b[^\n;]{0,60}(?:min|clamp|cap|freq|frecency)"
    r"|const\s+[A-Z0-9_]*(?:FREQ|FRECENCY|BONUS)[A-Z0-9_]*\s*:\s*\w+\s*=\s*15(?:\.0)?\b",
    re.IGNORECASE,
)


def _read_mod(rel_path: str) -> str:
    p = REPO_ROOT / rel_path
    assert p.is_file(), f"Missing Slice 3 module: {rel_path}"
    return production_rust_source(p.read_text(encoding="utf-8"))


@e2e_test(
    id="T2-S3-01",
    tier=2,
    area="slice3",
    features=("F20",),
    title="Deleting non-empty workspace requires redirect target & Alt+1 Starred mirror is protected",
    baseline=False,
)
def test_slice3_edge_workspace_delete_and_starred_protection() -> None:
    ws_src = _read_mod("crates/multibot-core/src/workspaces.rs")
    assert "starred" in ws_src.lower() or "favorite" in ws_src.lower(), (
        "workspaces.rs must protect Alt+1 Starred mirror from deletion"
    )
    assert "delete" in ws_src.lower() or "remove" in ws_src.lower(), (
        "workspaces.rs must implement workspace deletion with non-empty redirect handling"
    )
    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="workspaces", min_expected=4
    )


@e2e_test(
    id="T2-S3-02",
    tier=2,
    area="slice3",
    features=("F26",),
    title="F2 / /rename rejects blank names, trims outer spaces, caps at 63 chars & sets manual lock",
    baseline=False,
)
def test_slice3_edge_rename_blank_63char_and_manual_lock() -> None:
    ws_src = _read_mod("crates/multibot-core/src/workspaces.rs")
    desktop_prod = read_crate_production_sources(REPO_ROOT / "crates" / "multibot-desktop" / "src")
    combined = ws_src + "\n" + desktop_prod
    assert re.search(r"\b63(?:_?(?:usize|u32|u16|u8))?\b", combined), (
        "Rename contract must enforce 63-character maximum length (word-bounded 63 in production code)"
    )
    assert "manual" in combined.lower() or "session_titles" in combined.lower(), (
        "Rename contract must set manual lock / persist session_titles"
    )
    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="workspaces", min_expected=4
    )


def _palette_ranking_source(pal_src: str, ws_src: str) -> tuple[str, bool]:
    """Combine palette.rs + workspaces.rs ONLY when palette.rs genuinely invokes the core ranking engine."""
    masked = mask_rust_string_and_char_literals(pal_src)
    has_stats = ("PaletteStats" in masked) or ("palette_stats" in masked.lower())
    has_real_call = False
    if has_stats:
        for m in re.finditer(r"\b(rank_palette|palette_frecency\w*)\s*\(", masked):
            callee = m.group(1)
            prefix = masked[: m.start()].rstrip()
            if re.search(r"\bfn$", prefix):
                continue
            if prefix.endswith("::"):
                if re.search(r"\bmultibot_core(?:::\w+)*\s*::$", prefix) and not re.search(
                    r"\w\s*::\s*multibot_core(?:::\w+)*\s*::$", prefix
                ):
                    has_real_call = True
                    break
                if (
                    re.search(r"\bworkspaces\s*::$", prefix)
                    and not re.search(r"::\s*workspaces\s*::$", prefix)
                    and not re.search(r"\bmod\s+workspaces\b", masked)
                    and re.search(
                        r"\buse\s+multibot_core\b[^;]*\bworkspaces\s*(?:[;,}]|\bas\b|::\s*\{[^}]*\bself\b)",
                        masked,
                    )
                ):
                    has_real_call = True
                    break
                continue
            if prefix.endswith("."):
                continue
            if not re.search(rf"\bfn\s+{re.escape(callee)}\b", masked) and re.search(
                rf"\buse\s+multibot_core\b[^;]*(?:\b{re.escape(callee)}\b|\*)", masked
            ):
                has_real_call = True
                break
    integrates_core = has_stats and has_real_call
    return ((pal_src + "\n" + ws_src) if integrates_core else pal_src), integrates_core


def _extract_fn_body(src: str, fn_name: str) -> str:
    """Extract the balanced `{ ... }` body of `fn <fn_name>` from production Rust source."""
    masked = mask_rust_string_and_char_literals(src)
    m = re.search(rf"\bfn\s+{re.escape(fn_name)}\b", masked)
    assert m is not None, f"Could not find `fn {fn_name}` in workspaces.rs"
    i = m.end()
    n = len(masked)
    pdepth = 0
    while i < n:
        ch = masked[i]
        if ch == "(":
            pdepth += 1
        elif ch == ")" and pdepth > 0:
            pdepth -= 1
        elif ch == "{" and pdepth == 0:
            body, _ = extract_rust_brace_block(src, i)
            return body
        i += 1
    raise AssertionError(f"Could not find opening '{{' for `fn {fn_name}` in workspaces.rs")


@e2e_test(
    id="T2-S3-03",
    tier=2,
    area="slice3",
    features=("F25",),
    title="Ctrl+K excludes current/snoozed/waiting from last-5 recents & caps frecency bonus at +15",
    baseline=False,
)
def test_slice3_edge_palette_exclusions_and_frecency_cap() -> None:
    pal_src = _read_mod("crates/multibot-desktop/src/palette.rs")
    ws_src = _read_mod("crates/multibot-core/src/workspaces.rs")
    ranking_src, integrates_core = _palette_ranking_source(pal_src, ws_src)
    assert _FRECENCY_CAP_15_RE.search(ranking_src), (
        "palette.rs production code (or integrated workspaces.rs ranking engine via rank_palette(...)/PaletteStats) "
        "must cap frecency frequency bonus at +15 (with min/clamp/frecency context)"
    )
    if integrates_core:
        rank_body = mask_rust_string_and_char_literals(
            _extract_fn_body(ws_src, "rank_palette")
        ).lower()
        assert "snooz" in rank_body and "waiting" in rank_body, (
            "workspaces.rs::rank_palette() body must explicitly exclude BOTH snoozed workspaces "
            "and waiting sessions from empty-query last-5 recents"
        )
    else:
        pal_masked = mask_rust_string_and_char_literals(pal_src).lower()
        assert "snooz" in pal_masked and "waiting" in pal_masked, (
            "palette.rs must explicitly exclude BOTH snoozed workspaces and waiting sessions "
            "from empty-query last-5 recents"
        )
    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="palette", min_expected=2
    )
    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="palette", min_expected=3
    )


@e2e_test(
    id="T2-S3-04",
    tier=2,
    area="slice3",
    features=("F24",),
    title="Inbox Frequent split enforces 24h input / 3x10s dwell rule & Never Show permanent exclusion",
    baseline=False,
)
def test_slice3_edge_inbox_frequent_dwell_and_never_show() -> None:
    inbox_src = _read_mod("crates/multibot-desktop/src/inbox.rs")
    ws_src = _read_mod("crates/multibot-core/src/workspaces.rs")
    combined = (inbox_src + "\n" + ws_src).lower()
    assert "never" in combined or "excluded" in combined or "inbox_excluded" in combined, (
        "Inbox must support Never Show / inbox_excluded permanent exclusion"
    )
    assert "dwell" in combined or "frequent" in combined, (
        "Inbox must implement Frequent split qualification (24h input or >=3x 10s dwell in 6h)"
    )
    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="inbox", min_expected=3
    )


@e2e_test(
    id="T2-S3-05",
    tier=2,
    area="slice3",
    features=("F22", "F24", "INV-3"),
    title="Ctrl+Shift+Up/Down group boundary guard, Ctrl+Shift+U non-persisted filter & AltGr safety",
    baseline=False,
)
def test_slice3_edge_reorder_boundary_unread_non_persisted_and_altgr() -> None:
    sb_src = _read_mod("crates/multibot-desktop/src/sidebar.rs")
    assert len([ln for ln in sb_src.splitlines() if ln.strip()]) > 40, (
        "sidebar.rs appears to be a stub"
    )
    settings_src = _read_mod("crates/multibot-core/src/settings.rs")
    assert "unread_only" not in settings_src, (
        "Ctrl+Shift+U unread-only filter must NOT be persisted in settings.json across restarts"
    )
    assert not find_ctrl_alt_bindings(REPO_ROOT), "AltGr Ctrl+Alt violation detected"
    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="sidebar", min_expected=3
    )
