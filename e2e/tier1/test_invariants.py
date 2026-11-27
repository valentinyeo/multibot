"""Tier 1 Hard Invariants (T1-INV-01..06) — all MUST PASS on Slice 1 baseline and post-M6."""

from __future__ import annotations

import re
import tomllib

from e2e.lib import REPO_ROOT
from e2e.lib.cargo_runner import CARGO
from e2e.lib.daemon_driver import DaemonDriver
from e2e.lib.runner import e2e_test
from e2e.lib.sandbox import IsolatedHome
from e2e.lib.static_checks import (
    SLICE1_REQUIRED_CHORDS,
    check_desktop_cargo_gpui_features,
    check_dxgi_patch_invariants,
    check_oled_theme_tokens,
    extract_rust_brace_block,
    mask_rust_string_and_char_literals,
    parse_main_keybindings,
    parse_multibot_keys_shortcuts,
    production_rust_source,
    scan_chat_chrome_violations,
    scan_update_copy_violations,
)


@e2e_test(
    id="T1-INV-01",
    tier=1,
    area="invariants",
    features=("INV-1",),
    title="Windows DXGI dependency & patch graph (0% NVIDIA, WARP fallback)",
    baseline=True,
)
def test_windows_dxgi_patch_graph() -> None:
    root_toml = tomllib.loads((REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    patches = root_toml.get("patch", {}).get("crates-io", {})
    gpui_patch = patches.get("gpui-pre-windows", {})
    assert gpui_patch.get("path") == "patches/gpui-pre-windows", (
        f"[patch.crates-io] gpui-pre-windows must point to patches/gpui-pre-windows, got {gpui_patch!r}"
    )

    check_dxgi_patch_invariants(REPO_ROOT)

    rc, tree_out, tree_err = CARGO.cargo_tree(
        "multibot-desktop", target="x86_64-pc-windows-msvc"
    )
    assert rc == 0, f"cargo tree (Windows target) failed: {tree_err.strip()[:240]}"
    assert "gpui-pre-windows" in tree_out and "patches/gpui-pre-windows" in tree_out, (
        "Windows cargo tree must resolve gpui-pre-windows to local patches/gpui-pre-windows"
    )
    assert "gpui-fast" not in tree_out, (
        "CRITICAL INV-1 VIOLATION: gpui-fast / gpui-fast-windows appeared in Windows dependency tree!"
    )

    CARGO.assert_crate_tests_pass("multibot-gpu", min_expected=5)


@e2e_test(
    id="T1-INV-02",
    tier=1,
    area="invariants",
    features=("INV-1",),
    title="No Optimus/PowerXpress exports & CLI --gpu configured before GPUI init",
    baseline=True,
)
def test_no_gpu_exports_and_cli_pre_init() -> None:
    check_dxgi_patch_invariants(REPO_ROOT)

    main_rs = (REPO_ROOT / "crates" / "multibot-desktop" / "src" / "main.rs").read_text(
        encoding="utf-8"
    )
    cleaned = production_rust_source(main_rs)

    assert "--gpu=" in cleaned and '"--gpu"' in cleaned, (
        "main.rs must parse both --gpu=<val> and --gpu <val> CLI overrides"
    )

    masked_cleaned = mask_rust_string_and_char_literals(cleaned)
    main_m = re.search(r"\bfn\s+main\s*\([^)]*\)[^{]*\{", masked_cleaned)
    assert main_m is not None, "Could not locate fn main() in crates/multibot-desktop/src/main.rs"
    main_code, _ = extract_rust_brace_block(masked_cleaned, main_m.end() - 1)

    cfg_calls = [
        cm
        for cm in re.finditer(r"\bconfigure_gpu_env_before_gpui\s*\(", main_code)
        if not re.search(r"\bfn\s+$", main_code[: cm.start()])
    ]
    run_m = re.search(r"\bgpui_kit\s*::\s*application\s*\(\s*\)\s*\.\s*run\s*\(", main_code)

    assert cfg_calls, (
        "configure_gpu_env_before_gpui(...) must be CALLED inside fn main() in main.rs"
    )
    assert run_m is not None, (
        "gpui_kit::application().run(...) not found inside fn main() in main.rs"
    )
    assert cfg_calls[0].start() < run_m.start(), (
        "configure_gpu_env_before_gpui(...) must be called inside fn main() strictly BEFORE gpui_kit::application().run(...)"
    )


@e2e_test(
    id="T1-INV-03",
    tier=1,
    area="invariants",
    features=("INV-2",),
    title="%APPDATA%\\Multibot isolation & read-only ZigShell coexistence",
    baseline=True,
)
def test_multibot_isolation_and_readonly_zigshel() -> None:
    CARGO.assert_crate_tests_pass("multibot-core", min_expected=2)

    settings_rs = (REPO_ROOT / "crates" / "multibot-core" / "src" / "settings.rs").read_text(
        encoding="utf-8"
    )
    assert '"Multibot"' in settings_rs, "settings.rs must store config under Multibot"

    zigshel_rs = (
        REPO_ROOT / "crates" / "multibot-core" / "src" / "zigshel_import.rs"
    ).read_text(encoding="utf-8")
    # Strip comments and #[cfg(test)] blocks before checking production code for write calls
    prod_zigshel = production_rust_source(zigshel_rs)
    forbidden_writes = re.findall(
        r"\b(?:fs::write|File::create|OpenOptions|remove_file|remove_dir|create_dir|rename)\b",
        prod_zigshel,
    )
    assert not forbidden_writes, (
        f"zigshel_import.rs production code must be strictly read-only; found {forbidden_writes}"
    )

    with IsolatedHome(prefix="mb_t1_inv03_") as sb:
        snap = sb.stage_zigshel_fixtures(readonly=True)
        sb.stage_fixture(
            "claude/basic_slice1.jsonl",
            ".claude/projects/-tmp-proj-basic/claude-basic-001.jsonl",
        )
        with DaemonDriver(sb) as dmn:
            ack = dmn.request({"op": "hello", "client_version": "0.1.0"}, "hello_ack")
            assert ack["protocol_version"] == 1
            sessions = dmn.request({"op": "list_sessions", "limit": 20}, "sessions")
            assert len(sessions["sessions"]) == 1
        sb.assert_unchanged(sb.zigshel_dir, snap)


@e2e_test(
    id="T1-INV-04",
    tier=1,
    area="invariants",
    features=("INV-3", "F28"),
    title="Canonical keyboard shortcuts contract & wired KeyBinding alignment",
    baseline=True,
)
def test_keyboard_shortcuts_contract() -> None:
    CARGO.assert_crate_tests_pass("multibot-keys", min_expected=2)

    entries = parse_multibot_keys_shortcuts(REPO_ROOT)
    ids = [str(e["id"]) for e in entries]
    keys = [str(e["gpui_key"]) for e in entries]
    assert len(ids) == len(set(ids)), "Duplicate shortcut IDs in multibot-keys"
    assert len(keys) == len(set(keys)), "Duplicate gpui_key chords in multibot-keys"

    key_set = set(keys)
    missing = sorted(SLICE1_REQUIRED_CHORDS - key_set)
    assert not missing, f"Missing required canonical chords in SHORTCUTS: {missing}"

    bound_keys = set(parse_main_keybindings(REPO_ROOT))
    wired_keys = {str(e["gpui_key"]) for e in entries if e["wired"]}
    unbound_wired = sorted(wired_keys - bound_keys)
    assert not unbound_wired, (
        f"Shortcuts marked wired: true in multibot-keys but missing KeyBinding::new in desktop: {unbound_wired}"
    )


@e2e_test(
    id="T1-INV-05",
    tier=1,
    area="invariants",
    features=("INV-4", "F2"),
    title="Lean gpui-kit footprint, single-PID sysinfo & OLED theme tokens",
    baseline=True,
)
def test_lean_footprint_and_oled_theme() -> None:
    check_desktop_cargo_gpui_features(REPO_ROOT)

    for target in (None, "x86_64-pc-windows-msvc"):
        rc, out, err = CARGO.cargo_tree("multibot-desktop", target=target)
        assert rc == 0, f"cargo tree (target={target}) failed: {err.strip()[:240]}"
        assert "tree-sitter" not in out, (
            f"Forbidden tree-sitter C grammar found in cargo tree (target={target})"
        )
        assert "gpui-fast" not in out, (
            f"Forbidden gpui-fast found in cargo tree (target={target})"
        )

    footprint_rs = (
        REPO_ROOT / "crates" / "multibot-core" / "src" / "footprint.rs"
    ).read_text(encoding="utf-8")
    assert "ProcessesToUpdate::Some" in footprint_rs, (
        "footprint.rs must refresh only own PID via ProcessesToUpdate::Some(&[pid])"
    )

    check_oled_theme_tokens(REPO_ROOT)
    CARGO.assert_crate_tests_pass("multibot-desktop", min_expected=2)


@e2e_test(
    id="T1-INV-06",
    tier=1,
    area="invariants",
    features=("INV-4", "F36", "F38"),
    title="Status-free chat message chrome, clean update copy & TEST_INFRA coverage",
    baseline=True,
)
def test_status_free_chrome_and_test_infra() -> None:
    chrome_violations = scan_chat_chrome_violations(REPO_ROOT)
    assert not chrome_violations, (
        f"Forbidden legacy chat message chrome found: {chrome_violations}"
    )

    copy_violations = scan_update_copy_violations(REPO_ROOT)
    assert not copy_violations, (
        f"Forbidden update duration wording found: {copy_violations}"
    )

    infra_md = REPO_ROOT / "TEST_INFRA.md"
    assert infra_md.is_file(), "TEST_INFRA.md must exist at repository root"
    infra_text = infra_md.read_text(encoding="utf-8")
    for inv in ("INV-1", "INV-2", "INV-3", "INV-4"):
        assert inv in infra_text, f"TEST_INFRA.md missing {inv} in coverage matrix"
    for n in range(1, 39):
        assert f"F{n}" in infra_text, f"TEST_INFRA.md missing feature F{n} in coverage matrix"
