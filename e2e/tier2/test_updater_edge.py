"""Tier 2 Priority 1 Auto-Updater & CI edge/adversarial tests (T2-UPD-01..08)."""

from __future__ import annotations

import re

from e2e.lib import REPO_ROOT
from e2e.lib.cargo_runner import CARGO
from e2e.lib.runner import e2e_test
from e2e.lib.static_checks import production_rust_source, read_crate_production_sources

_SIZE_CAP_256MIB_RE = re.compile(
    r"\b256(?:_?(?:u64|usize|u32|i64))?\s*\*\s*1024(?:_?(?:u64|usize|u32|i64))?\s*\*\s*1024(?:_?(?:u64|usize|u32|i64))?\b"
    r"|\b256(?:_?(?:u64|usize|u32|i64))?\s*<<\s*20\b"
    r"|\b268_?435_?456(?:_?(?:u64|usize|u32|i64))?\b"
    r"|\bMAX_(?:DOWNLOAD|UPDATE|ARCHIVE|ASSET|ZIP|PAYLOAD)_(?:SIZE|BYTES)\b"
)

_TAMPER_TEST_SYNONYMS = (
    "tamper",
    "mismatch",
    "corrupt",
    "bitflip",
    "bad_sha",
    "wrong_sha",
    "invalid_sha",
    "bad_hash",
    "wrong_hash",
    "checksum_fail",
    "digest_mismatch",
    "hash_mismatch",
    "sha256_mismatch",
    "verify_fail",
    "rejects_bad",
)


def _read_updater_prod() -> str:
    return read_crate_production_sources(REPO_ROOT / "crates" / "multibot-updater" / "src")


@e2e_test(
    id="T2-UPD-01",
    tier=2,
    area="updater_ci",
    features=("F5",),
    title="HTTP 403 rate-limit backoff (Retry-After & X-RateLimit-Reset) & offline silent fail",
    baseline=False,
)
def test_updater_http_403_rate_limit_backoff() -> None:
    src = _read_updater_prod()
    assert "retry-after" in src.lower(), (
        "multibot-updater production code must inspect Retry-After header on HTTP 403"
    )
    assert "x-ratelimit-reset" in src.lower(), (
        "multibot-updater production code must inspect X-RateLimit-Reset header on HTTP 403"
    )
    assert re.search(r"\b403\b|\bFORBIDDEN\b|\b429\b|\bTOO_MANY_REQUESTS\b", src), (
        "multibot-updater production code must handle HTTP 403/429 rate-limit responses"
    )

    CARGO.assert_crate_tests_pass(
        "multibot-updater",
        min_expected=8,
        required_any_of=[
            ("403", "rate_limit", "ratelimit", "retry_after", "backoff", "forbidden", "offline"),
        ],
    )


@e2e_test(
    id="T2-UPD-02",
    tier=2,
    area="updater_ci",
    features=("F6",),
    title="SemVer ordering edge cases (0.1.10 > 0.1.9, leading v, equal & downgrade rejected)",
    baseline=False,
)
def test_updater_semver_edge_cases() -> None:
    src = _read_updater_prod()
    assert "semver" in src.lower() or "parse_version" in src.lower() or "Version" in src, (
        "multibot-updater must implement strict numeric SemVer comparison"
    )
    CARGO.assert_crate_tests_pass(
        "multibot-updater",
        min_expected=6,
        required_any_of=[("semver", "version_comparison", "newer_version", "version")],
    )


@e2e_test(
    id="T2-UPD-03",
    tier=2,
    area="updater_ci",
    features=("F6",),
    title="Draft & pre-release filtering (draft: true, prerelease: true, -rc.1/-beta.1)",
    baseline=False,
)
def test_updater_draft_and_prerelease_filtering() -> None:
    src = _read_updater_prod()
    assert re.search(r"\bdraft\b", src) and re.search(r"\bprerelease\b", src), (
        "multibot-updater production code must filter out draft and prerelease GitHub releases"
    )

    CARGO.assert_crate_tests_pass(
        "multibot-updater",
        min_expected=8,
        required_any_of=[
            ("draft", "filter_release", "select_release", "skip_draft", "release_filter"),
        ],
    )


@e2e_test(
    id="T2-UPD-04",
    tier=2,
    area="updater_ci",
    features=("F7",),
    title="Tampered SHA-256 rejection for both outer zip bitflip & inner multibot.exe mismatch",
    baseline=False,
)
def test_updater_tampered_sha256_rejection() -> None:
    src = _read_updater_prod()
    assert (
        re.search(
            r"\.(?:multibot_exe_sha256|multibotd_linux_x86_64_sha256|sha256)\b|\bSHA256SUMS\b",
            src,
        )
        or len(re.findall(r"Sha256::new\s*\(", src)) >= 2
    ), "multibot-updater production code must verify SHA-256 checksums of zip and extracted binaries"

    CARGO.assert_crate_tests_pass(
        "multibot-updater",
        min_expected=8,
        required_any_of=[_TAMPER_TEST_SYNONYMS],
    )


@e2e_test(
    id="T2-UPD-05",
    tier=2,
    area="updater_ci",
    features=("F7",),
    title="Zip-slip traversal (../ & ..\\), >256 MB cap & HTTPS host allowlist enforcement",
    baseline=False,
)
def test_updater_zip_slip_size_cap_and_host_allowlist() -> None:
    src = _read_updater_prod()
    assert any(t in src for t in ("ParentDir", "enclosed_name", "is_safe_zip_entry_path")), (
        "multibot-updater production code must explicitly guard against zip-slip path traversal"
    )
    assert _SIZE_CAP_256MIB_RE.search(src), (
        "multibot-updater production code must enforce a 256 MiB download size cap (not bare '256'/Sha256)"
    )
    assert "github.com" in src and "githubusercontent.com" in src, (
        "multibot-updater production code must enforce HTTPS GitHub host allowlist"
    )

    CARGO.assert_crate_tests_pass(
        "multibot-updater",
        min_expected=8,
        required_any_of=[
            ("zip_entry", "zip_slip", "traversal", "unsafe_zip", "path_traversal"),
            ("allowlist", "https_url", "redirect", "evil_host", "untrusted_host"),
            (
                "size_cap",
                "max_size",
                "max_bytes",
                "oversize",
                "too_large",
                "256mb",
                "256mib",
                "256_mb",
                "256_mib",
                "content_length",
                "download_limit",
                "size_limit",
                "exceeds_max",
            ),
        ],
    )


@e2e_test(
    id="T2-UPD-06",
    tier=2,
    area="updater_ci",
    features=("F6", "F8"),
    title="Partial-swap rollback, --update-ready failure & rejected.txt poison-pill tracking",
    baseline=False,
)
def test_updater_partial_swap_rollback_and_rejected_poison_pill() -> None:
    src = _read_updater_prod()
    for tok in ("rejected.txt", "--update-ready"):
        assert tok in src, f"multibot-updater missing required rollback/readiness token: {tok}"
    assert "rollback" in src.lower() or "restore" in src.lower(), (
        "multibot-updater must restore previous binary on partial-swap failure"
    )
    CARGO.assert_crate_tests_pass(
        "multibot-updater",
        min_expected=8,
        required_any_of=[
            ("rollback", "restore", "partial_swap", "swap_fail"),
            ("rejected", "poison", "update_ready", "readiness"),
        ],
    )


@e2e_test(
    id="T2-UPD-07",
    tier=2,
    area="updater_ci",
    features=("F6", "F9"),
    title="Dismissed update version persistence & superseding by newer SemVer release",
    baseline=False,
)
def test_updater_dismissed_version_superseded() -> None:
    settings_src = production_rust_source(
        (REPO_ROOT / "crates" / "multibot-core" / "src" / "settings.rs").read_text(
            encoding="utf-8"
        )
    )
    assert "dismissed_update_version" in settings_src, (
        "MultibotSettings must persist dismissed_update_version"
    )
    update_ui = REPO_ROOT / "crates" / "multibot-desktop" / "src" / "update_ui.rs"
    assert update_ui.is_file(), "crates/multibot-desktop/src/update_ui.rs not found"
    ui_src = production_rust_source(update_ui.read_text(encoding="utf-8"))
    assert "dismiss" in ui_src.lower(), (
        "update_ui.rs must track dismissed version and re-surface when a newer release arrives"
    )
    CARGO.assert_crate_tests_pass("multibot-desktop", filter_expr="update", min_expected=1)


@e2e_test(
    id="T2-UPD-08",
    tier=2,
    area="updater_ci",
    features=("F10",),
    title="Remote multibotd deploy atomicity (.tmp + mv -f + ln -sf) & same-version hash drift upgrade",
    baseline=False,
)
def test_remote_daemon_deploy_atomicity_and_hash_drift() -> None:
    deploy_rs = REPO_ROOT / "crates" / "multibot-core" / "src" / "daemon_deploy.rs"
    assert deploy_rs.is_file(), "crates/multibot-core/src/daemon_deploy.rs not found"
    src = production_rust_source(deploy_rs.read_text(encoding="utf-8"))
    assert ".tmp" in src and "mv -f" in src and "ln -sf" in src, (
        "daemon_deploy.rs must atomically stage .tmp -> mv -f -> ln -sf without overwriting live binary in place"
    )
    assert "binary_hash" in src or "sha" in src.lower(), (
        "daemon_deploy.rs must detect same-version binary_hash drift and trigger upgrade"
    )
    CARGO.assert_crate_tests_pass(
        "multibot-core", filter_expr="daemon_deploy", min_expected=2
    )
