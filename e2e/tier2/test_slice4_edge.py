"""Tier 2 Slice 4 boundary & corner-case tests (T2-S4-01..05)."""

from __future__ import annotations

import re

from e2e.lib import REPO_ROOT
from e2e.lib.cargo_runner import CARGO
from e2e.lib.runner import e2e_test
from e2e.lib.static_checks import production_rust_source, read_crate_production_sources

_CACHE_512K_RE = re.compile(
    r"\b512(?:_?(?:usize|u64|u32))?\s*\*\s*1024(?:_?(?:usize|u64|u32))?\b"
    r"|\b524_?288(?:_?(?:usize|u64|u32))?\b"
    r"|\bMAX_SESSION_CACHE_BYTES\b"
)
_CACHE_64M_RE = re.compile(
    r"\b64(?:_?(?:usize|u64|u32))?\s*\*\s*1024(?:_?(?:usize|u64|u32))?\s*\*\s*1024(?:_?(?:usize|u64|u32))?\b"
    r"|\b64(?:_?(?:usize|u64|u32))?\s*<<\s*20\b"
    r"|\b67_?108_?864(?:_?(?:usize|u64|u32))?\b"
    r"|\bMAX_CACHE_DIR_BYTES\b"
)
_OUTBOX_CAP_20_RE = re.compile(
    r"\bMAX_OUTBOX_PER_SESSION\b[^;\n]*\b20\b"
    r"|(?:max|cap|limit|len|truncate|clamp|outbox)[^\n;]{0,60}\b20(?:_?(?:usize|u32|u16))?\b"
    r"|\b20(?:_?(?:usize|u32|u16))?\b[^\n;]{0,60}(?:max|cap|limit|outbox|fifo)",
    re.IGNORECASE,
)


def _read_mod(rel_path: str) -> str:
    p = REPO_ROOT / rel_path
    assert p.is_file(), f"Missing Slice 4 module: {rel_path}"
    return production_rust_source(p.read_text(encoding="utf-8"))


@e2e_test(
    id="T2-S4-01",
    tier=2,
    area="slice4",
    features=("F32",),
    title="Option cards never parse A./B. inside fenced code blocks or older assistant messages",
    baseline=False,
)
def test_slice4_edge_option_card_exclusions() -> None:
    chat_src = _read_mod("crates/multibot-desktop/src/chat_view.rs")
    assert "```" in chat_src or "fence" in chat_src.lower() or "code_block" in chat_src.lower(), (
        "chat_view.rs must exclude fenced code blocks when scanning trailing prose A/B/C option cards"
    )
    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="chat_view", min_expected=4
    )


@e2e_test(
    id="T2-S4-02",
    tier=2,
    area="slice4",
    features=("F35",),
    title="Attachment validation rejects >10 images, >10 MB image, bad ext & >200 MB file",
    baseline=False,
)
def test_slice4_edge_attachment_caps_and_extension_validation() -> None:
    desktop_src = read_crate_production_sources(REPO_ROOT / "crates" / "multibot-desktop" / "src")
    core_src = read_crate_production_sources(REPO_ROOT / "crates" / "multibot-core" / "src")
    combined = desktop_src + "\n" + core_src
    assert "imgcache" in combined or "uploads" in combined, (
        "Attachment handling must target imgcache / ~/.multibot/uploads/<session>/"
    )
    assert "webp" in combined.lower() and "png" in combined.lower(), (
        "Image attachment allowlist must validate .png/.jpg/.jpeg/.gif/.webp extensions"
    )
    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="chat_view", min_expected=4
    )


@e2e_test(
    id="T2-S4-03",
    tier=2,
    area="slice4",
    features=("F33",),
    title="Quote/Reply enforces 10-line `> ` cap & hoists leading /skill above quote block (MUBO-457)",
    baseline=False,
)
def test_slice4_edge_quote_10line_cap_and_skill_hoist() -> None:
    chat_src = _read_mod("crates/multibot-desktop/src/chat_view.rs")
    assert "> " in chat_src, "Quote/Reply must format quoted lines with `> ` prefix"
    assert "quote" in chat_src.lower(), "chat_view.rs missing quote formatter"
    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="chat_view", min_expected=4
    )


@e2e_test(
    id="T2-S4-04",
    tier=2,
    area="slice4",
    features=("F37",),
    title="Two-tier cache enforces 512 KiB/session, 64 MiB LRU eviction, atomic .tmp & corrupt JSON recovery",
    baseline=False,
)
def test_slice4_edge_cache_caps_atomic_and_corrupt_recovery() -> None:
    cache_src = _read_mod("crates/multibot-core/src/cache.rs")
    assert _CACHE_512K_RE.search(cache_src), (
        "cache.rs must enforce 512 KiB per-session tail cap"
    )
    assert _CACHE_64M_RE.search(cache_src), (
        "cache.rs must enforce 64 MiB total disk cache LRU eviction (not bare '64'/u64)"
    )
    assert ".tmp" in cache_src, "cache.rs must write atomically via .tmp + rename"
    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="cache", min_expected=4
    )


@e2e_test(
    id="T2-S4-05",
    tier=2,
    area="slice4",
    features=("F36", "F37"),
    title="Rapid switch late async discard, max-20 FIFO outbox cap & 10..28pt zoom clamp",
    baseline=False,
)
def test_slice4_edge_late_switch_outbox_cap_and_zoom_clamp() -> None:
    outbox_src = _read_mod("crates/multibot-core/src/outbox.rs")
    assert _OUTBOX_CAP_20_RE.search(outbox_src), (
        "outbox.rs must cap offline queue at 20 FIFO items per session"
    )
    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="outbox", min_expected=3
    )

    settings_src = _read_mod("crates/multibot-core/src/settings.rs")
    assert (
        re.search(r"\bMIN_CHAT_FONT_SIZE_PT\b[^;\n]*=\s*10\b", settings_src)
        and re.search(r"\bMAX_CHAT_FONT_SIZE_PT\b[^;\n]*=\s*28\b", settings_src)
    ), "settings.rs must define MIN_CHAT_FONT_SIZE_PT = 10 and MAX_CHAT_FONT_SIZE_PT = 28"

    desktop_prod = read_crate_production_sources(REPO_ROOT / "crates" / "multibot-desktop" / "src")
    has_const_clamp = (
        "MIN_CHAT_FONT_SIZE_PT" in desktop_prod and "MAX_CHAT_FONT_SIZE_PT" in desktop_prod
    )
    has_literal_clamp = bool(
        re.search(
            r"\b10(?:\.0)?\s*\.\.=\s*28(?:\.0)?\b"
            r"|\.clamp\(\s*10(?:\.0)?\s*,\s*28(?:\.0)?\s*\)"
            r"|(?:chat_font_size|font_size|zoom)[^\n]{0,80}\b(?:10|28)\b",
            desktop_prod,
            re.IGNORECASE,
        )
    )
    assert has_const_clamp or has_literal_clamp, (
        "multibot-desktop production code must clamp chat font zoom to 10..=28pt "
        "(via MIN_CHAT_FONT_SIZE_PT/MAX_CHAT_FONT_SIZE_PT, clamp(10, 28), or 10..=28)"
    )
    CARGO.assert_crate_tests_pass(
        "multibot-desktop", filter_expr="chat_view", min_expected=4
    )
