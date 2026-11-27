"""Tier 1 Slice 4 tests (T1-S4-01..05): Rich Chat View, Tables, Diffs/Todos, Option Cards, Quote/Reactions, Picker/Outbox/Zoom & <16ms Cache."""

from __future__ import annotations

import re

from e2e.lib import REPO_ROOT
from e2e.lib.cargo_runner import CARGO
from e2e.lib.runner import e2e_test
from e2e.lib.static_checks import (
    check_desktop_cargo_gpui_features,
    production_rust_source,
    scan_chat_chrome_violations,
)


def _read_required_module(rel_path: str) -> str:
    p = REPO_ROOT / rel_path
    assert p.is_file(), f"Missing Slice 4 module: {rel_path}"
    return production_rust_source(p.read_text(encoding="utf-8"))


@e2e_test(
    id="T1-S4-01",
    tier=1,
    area="slice4",
    features=("F29", "F30"),
    title="Markdown + pure-Rust syntect code blocks (Ctrl+Shift+D) & expandable GFM/Unicode tables",
    baseline=False,
)
def test_slice4_markdown_syntect_and_expandable_tables() -> None:
    feats = check_desktop_cargo_gpui_features(REPO_ROOT)
    assert "syntect" in feats, (
        f"multibot-desktop Cargo.toml must enable pure-Rust `syntect` feature on gpui-kit; found {sorted(feats)}"
    )

    chat_src = _read_required_module("crates/multibot-desktop/src/chat_view.rs")
    table_src = _read_required_module("crates/multibot-desktop/src/table_modal.rs")
    assert len(chat_src.splitlines()) > 40, "chat_view.rs appears to be an empty stub"
    assert len(table_src.splitlines()) > 20, "table_modal.rs appears to be an empty stub"

    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="table", min_expected=1
    )
    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="chat_view", min_expected=2
    )


@e2e_test(
    id="T1-S4-02",
    tier=1,
    area="slice4",
    features=("F31", "F32"),
    title="Collapsible tool steps, inline unified diffs, todo drawer & interactive option cards",
    baseline=False,
)
def test_slice4_tools_diffs_todos_and_option_cards() -> None:
    chat_src = _read_required_module("crates/multibot-desktop/src/chat_view.rs")
    low = chat_src.lower()
    for tok in ("diff", "todo", "question", "option"):
        assert tok in low, (
            f"chat_view.rs missing required rich block support token: {tok!r}"
        )

    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="chat_view", min_expected=3
    )


@e2e_test(
    id="T1-S4-03",
    tier=1,
    area="slice4",
    features=("F33", "F34"),
    title="Quote/Reply (hoist /skill, 10-line cap), message keyboard nav & emoji reactions",
    baseline=False,
)
def test_slice4_quote_reply_and_reactions() -> None:
    chat_src = _read_required_module("crates/multibot-desktop/src/chat_view.rs")
    low = chat_src.lower()
    assert "quote" in low or "reply" in low, (
        "chat_view.rs must implement Quote/Reply handling"
    )
    assert "reaction" in low or "\U0001f44d" in chat_src, (
        "chat_view.rs must implement message emoji reactions"
    )

    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="chat_view", min_expected=3
    )


@e2e_test(
    id="T1-S4-04",
    tier=1,
    area="slice4",
    features=("F35", "F36"),
    title="Image paste/drop & thumbnails, / & $ slash picker, Ctrl+F find, chat-outbox.json & zoom",
    baseline=False,
)
def test_slice4_attachments_slash_picker_outbox_and_zoom() -> None:
    slash_src = _read_required_module("crates/multibot-desktop/src/slash_picker.rs")
    outbox_src = _read_required_module("crates/multibot-core/src/outbox.rs")
    settings_src = _read_required_module("crates/multibot-core/src/settings.rs")

    assert len([ln for ln in slash_src.splitlines() if ln.strip()]) > 20, (
        "slash_picker.rs appears to be an empty stub"
    )
    assert re.search(r'fn\s+outbox_path\b[^}]*"chat-outbox\.json"', settings_src), (
        "MultibotSettings::outbox_path() must map to canonical chat-outbox.json in settings.rs"
    )
    assert any(
        t in outbox_src
        for t in ("chat-outbox.json", "outbox_path", "save_to", "load_from", "std::fs")
    ), (
        "crates/multibot-core/src/outbox.rs must implement persistence (chat-outbox.json, MultibotSettings::outbox_path(), or Path load/save)"
    )
    assert "chat_font_size_pt" in settings_src, (
        "MultibotSettings must persist chat_font_size_pt for Ctrl++/-/0 zoom"
    )

    chrome_violations = scan_chat_chrome_violations(REPO_ROOT)
    assert not chrome_violations, f"Status-free chrome violation: {chrome_violations}"

    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="outbox", min_expected=2
    )
    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="slash", min_expected=1
    )


@e2e_test(
    id="T1-S4-05",
    tier=1,
    area="slice4",
    features=("F37",),
    title="<16ms two-tier local chat cache (in-memory LRU + chatcache/<sha256>.jsonl + probe)",
    baseline=False,
)
def test_slice4_two_tier_local_chat_cache() -> None:
    cache_src = _read_required_module("crates/multibot-core/src/cache.rs")
    settings_src = _read_required_module("crates/multibot-core/src/settings.rs")
    assert re.search(r'fn\s+chat_cache_dir\b[^}]*"chatcache"', settings_src), (
        "MultibotSettings::chat_cache_dir() must map to canonical chatcache dir in settings.rs"
    )
    assert (
        "chatcache" in cache_src.lower()
        or "chat_cache_dir" in cache_src
        or "ChatCache::new" in cache_src
        or re.search(r"\bfn\s+new\s*\([^)]*PathBuf", cache_src)
    ), "crates/multibot-core/src/cache.rs must persist disk cache via chatcache or MultibotSettings::chat_cache_dir()"
    for tok in ("probe", ".tmp"):
        assert tok in cache_src.lower(), (
            f"crates/multibot-core/src/cache.rs missing two-tier cache contract token: {tok!r}"
        )

    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="cache", min_expected=3
    )
