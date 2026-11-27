"""Tier 2 Hard Invariants edge & corner-case tests (T2-INV-01..06)."""

from __future__ import annotations

import re

from e2e.lib import REPO_ROOT
from e2e.lib.cargo_runner import CARGO
from e2e.lib.daemon_driver import DaemonDriver
from e2e.lib.runner import e2e_test
from e2e.lib.sandbox import IsolatedHome
from e2e.lib.static_checks import (
    PHASE2_RESERVED_CHORDS,
    SLICE34_REQUIRED_WIRED_CHORDS,
    check_conditional_footprint_notify,
    find_ctrl_alt_bindings,
    parse_main_keybindings,
    parse_multibot_keys_shortcuts,
    production_rust_source,
    scan_update_copy_violations,
)


@e2e_test(
    id="T2-INV-01",
    tier=2,
    area="invariants",
    features=("INV-3", "F28"),
    title="AltGr safety: zero Ctrl+Alt bindings anywhere in multibot-keys or desktop",
    baseline=True,
)
def test_altgr_ctrl_alt_strictly_forbidden() -> None:
    violations = find_ctrl_alt_bindings(REPO_ROOT)
    assert not violations, (
        f"AltGr violation: Ctrl+Alt chord(s) bound in shortcuts/desktop: {violations}"
    )


@e2e_test(
    id="T2-INV-02",
    tier=2,
    area="invariants",
    features=("INV-1",),
    title="Unpatched gpui-fast-windows lockfile guardrail & missing-LUID fallback to WARP (never 0x10DE)",
    baseline=True,
)
def test_gpui_fast_lockfile_guardrail_and_missing_luid_fallback() -> None:
    rc, meta, err = CARGO.cargo_metadata()
    assert rc == 0 and meta is not None, f"cargo metadata failed: {err.strip()[:200]}"
    rc_t, tree_out, tree_err = CARGO.cargo_tree(
        "multibot-desktop", target="x86_64-pc-windows-msvc"
    )
    assert rc_t == 0, f"cargo tree (Windows) failed: {tree_err.strip()[:200]}"
    assert "gpui-fast-windows" not in tree_out and "gpui-fast" not in tree_out, (
        "CRITICAL: unpatched gpui-fast-windows is active in multibot-desktop Windows graph!"
    )

    dxgi = (
        REPO_ROOT / "patches" / "gpui-pre-windows" / "src" / "directx_devices.rs"
    ).read_text(encoding="utf-8")
    assert "specific_luid" in dxgi and "is_nvidia" in dxgi and "is_software" in dxgi, (
        "DXGI patch must fall back from vanished specific_luid to non-NVIDIA hardware -> WARP"
    )
    CARGO.assert_crate_tests_pass("multibot-gpu", min_expected=5)


@e2e_test(
    id="T2-INV-03",
    tier=2,
    area="invariants",
    features=("INV-2", "F27"),
    title="Legacy ZigShell & multiprompt byte-for-byte immutability under daemon & importer activity",
    baseline=True,
)
def test_legacy_zigshel_and_multiprompt_immutability() -> None:
    # Verify zero production write paths target ZigShell or /multiprompt across multibot-core & multibotd
    write_call_re = re.compile(
        r"\b(?:fs::write|File::create|OpenOptions|remove_file|remove_dir|create_dir|rename)\b"
    )
    for crate_name in ("multibot-core", "multibotd"):
        src_dir = REPO_ROOT / "crates" / crate_name / "src"
        for rs in sorted(src_dir.rglob("*.rs")):
            prod = production_rust_source(rs.read_text(encoding="utf-8"))
            if "ZigShell" in prod or "multiprompt" in prod:
                # Only allow read-only calls in files mentioning ZigShell
                if "ZigShell" in prod and write_call_re.search(prod):
                    # Verify write calls are not applied to zigshel paths
                    for line in prod.splitlines():
                        if ("zigshel" in line.lower() or "ZigShell" in line) and write_call_re.search(line):
                            raise AssertionError(
                                f"Forbidden write referencing ZigShell in {rs.relative_to(REPO_ROOT)}: {line.strip()}"
                            )

    with IsolatedHome(prefix="mb_t2_inv03_") as sb:
        snap = sb.stage_zigshel_fixtures(readonly=True)
        mp_dir = sb.home / ".multiprompt"
        mp_dir.mkdir(parents=True, exist_ok=True)
        (mp_dir / "legacy.json").write_text('{"legacy":true}\n', encoding="utf-8")
        mp_snap = sb.lock_readonly(mp_dir)

        sb.stage_fixture(
            "claude/basic_slice1.jsonl",
            ".claude/projects/-tmp-proj-basic/claude-basic-001.jsonl",
        )
        sb.stage_live_legacy_chat(tmux="live-basic-001", sid="claude-basic-001")
        with DaemonDriver(sb) as dmn:
            dmn.request({"op": "hello", "client_version": "0.1.0"}, "hello_ack")
            dmn.request({"op": "list_sessions", "limit": 10}, "sessions")
            dmn.request({"op": "ping", "nonce": 1}, "pong")

        sb.assert_unchanged(sb.zigshel_dir, snap)
        sb.assert_unchanged(mp_dir, mp_snap)


@e2e_test(
    id="T2-INV-04",
    tier=2,
    area="invariants",
    features=("INV-4", "F2"),
    title="Demand-driven rendering: zero 60 FPS spin & conditional footprint cx.notify()",
    baseline=False,
)
def test_demand_driven_conditional_footprint_notify() -> None:
    check_conditional_footprint_notify(REPO_ROOT)


@e2e_test(
    id="T2-INV-05",
    tier=2,
    area="invariants",
    features=("INV-4", "F9"),
    title="Update Card zero-duration copy guardrail (strict string-literal regex)",
    baseline=False,
)
def test_update_card_zero_duration_copy_guardrail() -> None:
    update_ui = REPO_ROOT / "crates" / "multibot-desktop" / "src" / "update_ui.rs"
    assert update_ui.is_file(), (
        "crates/multibot-desktop/src/update_ui.rs not found (expected in M2)"
    )
    src = update_ui.read_text(encoding="utf-8")
    assert "Update & Restart" in src and "is ready" in src, (
        "update_ui.rs appears to be a stub without real Update Card UI copy"
    )
    violations = scan_update_copy_violations(REPO_ROOT)
    assert not violations, f"Forbidden duration wording in update_ui.rs: {violations}"


@e2e_test(
    id="T2-INV-06",
    tier=2,
    area="invariants",
    features=("INV-3", "F28"),
    title="All Slice 3/4 shortcuts wired without conflict with Phase 2 reserved chords",
    baseline=False,
)
def test_slice34_shortcuts_wired_without_phase2_conflicts() -> None:
    entries = parse_multibot_keys_shortcuts(REPO_ROOT)
    by_key = {str(e["gpui_key"]): e for e in entries}
    wired_keys = {k for k, e in by_key.items() if e["wired"]}
    missing = sorted(SLICE34_REQUIRED_WIRED_CHORDS - wired_keys)
    assert not missing, f"Slice 3/4 shortcuts not yet wired: true in multibot-keys: {missing}"

    bound = set(parse_main_keybindings(REPO_ROOT))
    unbound = sorted(SLICE34_REQUIRED_WIRED_CHORDS - bound)
    assert not unbound, f"Slice 3/4 shortcuts missing KeyBinding::new in desktop: {unbound}"

    conflicts = sorted(SLICE34_REQUIRED_WIRED_CHORDS & PHASE2_RESERVED_CHORDS)
    assert not conflicts, f"Slice 3/4 chord collides with Phase 2 reserved chords: {conflicts}"

    # Phase 2 reserved chords must still exist in SHORTCUTS registry
    missing_reserved = sorted(PHASE2_RESERVED_CHORDS - set(by_key.keys()))
    assert not missing_reserved, (
        f"Phase 2 reserved chords removed from multibot-keys: {missing_reserved}"
    )
