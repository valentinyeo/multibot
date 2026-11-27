"""Tier 1 Slice 3 tests (T1-S3-01..05): Workspaces, Favorites, Sidebar, Inbox, Palette, Rename, SharedAppStore & Keys."""

from __future__ import annotations

import re

from e2e.lib import REPO_ROOT
from e2e.lib.cargo_runner import CARGO
from e2e.lib.runner import e2e_test
from e2e.lib.static_checks import (
    SLICE34_REQUIRED_WIRED_CHORDS,
    mask_rust_string_and_char_literals,
    parse_main_keybindings,
    parse_multibot_keys_shortcuts,
    production_rust_source,
    read_crate_production_sources,
)


def _read_required_module(rel_path: str) -> str:
    p = REPO_ROOT / rel_path
    assert p.is_file(), f"Missing Slice 3 module: {rel_path}"
    return production_rust_source(p.read_text(encoding="utf-8"))


@e2e_test(
    id="T1-S3-01",
    tier=1,
    area="slice3",
    features=("F20", "F21"),
    title="Workspaces (Alt+1..9 Starred mirror, snooze, CRUD) & Favorites (Alt+S) persisted in workspaces.json",
    baseline=False,
)
def test_slice3_workspaces_and_favorites() -> None:
    ws_src = _read_required_module("crates/multibot-core/src/workspaces.rs")
    settings_src = _read_required_module("crates/multibot-core/src/settings.rs")
    assert re.search(r'fn\s+workspaces_path\b[^}]*"workspaces\.json"', settings_src), (
        "MultibotSettings::workspaces_path() must map to canonical workspaces.json in settings.rs"
    )
    assert (
        "workspaces.json" in ws_src.lower()
        or "workspaces_path" in ws_src
        or ("load_from" in ws_src and "save_to" in ws_src)
    ), "workspaces.rs must persist via workspaces.json, MultibotSettings::workspaces_path(), or load_from/save_to"
    for tok in ("favorite", "snooz"):
        assert tok in ws_src.lower(), (
            f"crates/multibot-core/src/workspaces.rs missing expected contract concept: {tok}"
        )

    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="workspaces", min_expected=3
    )


@e2e_test(
    id="T1-S3-02",
    tier=1,
    area="slice3",
    features=("F22", "F23"),
    title="Groups (Ctrl+G), Pinned/Recents auto-sort, reorder & 3-state Sidebar cycle (Ctrl+Shift+B)",
    baseline=False,
)
def test_slice3_groups_pinned_recents_and_3state_sidebar() -> None:
    sb_src = _read_required_module("crates/multibot-desktop/src/sidebar.rs")
    settings_src = _read_required_module("crates/multibot-core/src/settings.rs")
    assert "sidebar_mode" in settings_src, (
        "MultibotSettings must persist sidebar_mode (Open / Collapsed / Hidden)"
    )
    combined = (sb_src + "\n" + settings_src).lower()
    for state_tok in ("open", "collapsed", "hidden"):
        assert state_tok in combined, (
            f"3-state sidebar cycle missing state {state_tok!r} in sidebar.rs/settings.rs"
        )

    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="sidebar", min_expected=2
    )


@e2e_test(
    id="T1-S3-03",
    tier=1,
    area="slice3",
    features=("F24",),
    title="Full Cross-Workspace Inbox (Frequent/All/Favorites/workspace splits, vim nav, Waiting+Undo)",
    baseline=False,
)
def test_slice3_cross_workspace_inbox() -> None:
    inbox_src = _read_required_module("crates/multibot-desktop/src/inbox.rs")
    low = inbox_src.lower()
    for tok in ("frequent", "favorites", "waiting"):
        assert tok in low, f"inbox.rs missing required Inbox split/feature token: {tok!r}"

    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="inbox", min_expected=2
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


@e2e_test(
    id="T1-S3-04",
    tier=1,
    area="slice3",
    features=("F25", "F26"),
    title="Ranked Ctrl+K Command Palette (last-5, frecency, palette-stats.json) & F2 inline rename",
    baseline=False,
)
def test_slice3_ranked_palette_and_inline_rename() -> None:
    pal_src = _read_required_module("crates/multibot-desktop/src/palette.rs")
    ws_src = _read_required_module("crates/multibot-core/src/workspaces.rs")
    settings_src = _read_required_module("crates/multibot-core/src/settings.rs")
    assert re.search(r'fn\s+palette_stats_path\b[^}]*"palette-stats\.json"', settings_src), (
        "MultibotSettings::palette_stats_path() must map to canonical palette-stats.json in settings.rs"
    )
    assert (
        "palette-stats.json" in pal_src
        or "palette_stats" in pal_src.lower()
        or ("load_from" in pal_src and "save_to" in pal_src)
    ), "palette.rs must persist command frecency to palette-stats.json or MultibotSettings::palette_stats_path()"

    ranking_src, _ = _palette_ranking_source(pal_src, ws_src)
    assert "most used" in ranking_src.lower() or "frecency" in ranking_src.lower(), (
        "palette.rs must implement Most used / frecency ranking directly or integrate with "
        "workspaces.rs ranking engine (rank_palette(...)/PaletteStats)"
    )

    desktop_all = read_crate_production_sources(REPO_ROOT / "crates" / "multibot-desktop" / "src")
    assert "RenameSession" in desktop_all, (
        "Desktop F2 / /rename must emit RenameSession to multibotd"
    )

    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="palette", min_expected=2
    )
    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="palette", min_expected=2
    )


@e2e_test(
    id="T1-S3-05",
    tier=1,
    area="slice3",
    features=("F27", "F28"),
    title="SharedAppStore multi-window state, expanded read-only ZigShell import & complete shortcuts",
    baseline=False,
)
def test_slice3_shared_store_zigshel_and_shortcuts() -> None:
    state_src = _read_required_module("crates/multibot-desktop/src/state.rs")
    assert "SharedAppStore" in state_src, (
        "crates/multibot-desktop/src/state.rs must define SharedAppStore GPUI Global"
    )

    zigshel_src = _read_required_module("crates/multibot-core/src/zigshel_import.rs")
    assert "workspace" in zigshel_src.lower() and "favorite" in zigshel_src.lower(), (
        "zigshel_import.rs must import workspaces & favorites read-only in addition to SSH hosts"
    )
    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="zigshel_import", min_expected=2
    )

    entries = parse_multibot_keys_shortcuts(REPO_ROOT)
    wired_keys = {str(e["gpui_key"]) for e in entries if e["wired"]}
    missing_wired = sorted(SLICE34_REQUIRED_WIRED_CHORDS - wired_keys)
    assert not missing_wired, (
        f"Slice 3/4 chords missing wired: true in multibot-keys: {missing_wired}"
    )
    bound_keys = set(parse_main_keybindings(REPO_ROOT))
    unbound = sorted(SLICE34_REQUIRED_WIRED_CHORDS - bound_keys)
    assert not unbound, (
        f"Slice 3/4 chords missing KeyBinding::new in multibot-desktop: {unbound}"
    )
    CARGO.assert_crate_tests_pass("multibot-keys", min_expected=2)
