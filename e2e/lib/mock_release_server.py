"""Ephemeral-port local HTTP mock GitHub Releases server with fault injection."""

from __future__ import annotations

import hashlib
import io
import json
import threading
import zipfile
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

from .daemon_driver import resolve_multibotd_binary


@dataclass
class ReleaseBundle:
    version: str
    tag: str
    zip_bytes: bytes
    zip_sha256: str
    multibot_exe_sha256: str
    multibotd_sha256: str
    sha256sums_txt: str
    manifest_json: Dict[str, Any]


@dataclass
class MockReleaseSpec:
    version: str = "0.1.99"
    tag: Optional[str] = None
    draft: bool = False
    prerelease: bool = False
    published_at: str = "2026-11-26T20:00:00Z"
    notes: List[str] = field(
        default_factory=lambda: [
            "Faster chat switching with two-tier local cache",
            "Automatic in-app updates with clean restart",
        ]
    )
    etag: Optional[str] = None
    tamper_mode: str = "none"
    # Supported tamper_mode values:
    # "none", "zip_bitflip", "inner_exe_mismatch", "zip_slip_posix",
    # "zip_slip_windows", "oversized_content_length", "evil_redirect"

    @property
    def effective_tag(self) -> str:
        return self.tag or f"v{self.version.lstrip('v')}"

    @property
    def effective_etag(self) -> str:
        return self.etag or f'"{self.effective_tag}"'


def _sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def build_release_bundle(
    version: str,
    *,
    base_url: str,
    notes: Optional[List[str]] = None,
    published_at: str = "2026-11-26T20:00:00Z",
    tamper_mode: str = "none",
    daemon_binary: Optional[Path] = None,
) -> ReleaseBundle:
    """Build a real multibot-windows-x64.zip + SHA256SUMS.txt + update-manifest.json."""
    clean_ver = version.lstrip("v")
    tag = f"v{clean_ver}"

    # Synthetic PE-stub multibot.exe with embedded version tag
    exe_bytes = b"MZ\x90\x00MULTIBOT_WIN_STUB_" + clean_ver.encode("ascii") + (b"\x00" * 512)
    try:
        bin_path = resolve_multibotd_binary(daemon_binary)
        daemon_bytes = bin_path.read_bytes()
    except FileNotFoundError:
        daemon_bytes = b"\x7fELFMULTIBOTD_STUB_" + clean_ver.encode("ascii") + (b"\x00" * 512)

    exe_sha = _sha256_hex(exe_bytes)
    daemon_sha = _sha256_hex(daemon_bytes)

    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        if tamper_mode == "zip_slip_posix":
            zf.writestr("../evil.exe", b"EVIL_POSIX_ESCAPE")
            zf.writestr("sub/../../escape.dll", b"EVIL_NESTED")
        elif tamper_mode == "zip_slip_windows":
            zf.writestr("..\\win_escape.exe", b"EVIL_WIN_ESCAPE")
            zf.writestr("/tmp/abs_evil", b"EVIL_ABS")
        if tamper_mode == "inner_exe_mismatch":
            tampered_exe = exe_bytes[:-1] + bytes([exe_bytes[-1] ^ 0xFF])
            zf.writestr("multibot.exe", tampered_exe)
        else:
            zf.writestr("multibot.exe", exe_bytes)
        zf.writestr("multibotd-linux-x86_64", daemon_bytes)

    zip_bytes = buf.getvalue()
    # Compute honest digest BEFORE zip_bitflip tampering so SHA256SUMS/manifest mismatch on flip
    honest_zip_sha = _sha256_hex(zip_bytes)
    if tamper_mode == "zip_bitflip":
        mut = bytearray(zip_bytes)
        mut[len(mut) // 2] ^= 0x01
        zip_bytes = bytes(mut)

    asset_name = "multibot-windows-x64.zip"
    sha256sums = "\n".join(
        [
            f"{honest_zip_sha}  {asset_name}",
            f"{exe_sha}  multibot.exe",
            f"{daemon_sha}  multibotd-linux-x86_64",
        ]
    ) + "\n"

    manifest = {
        "schema_version": 1,
        "version": clean_ver,
        "tag": tag,
        "published_at": published_at,
        "notes": notes or [f"Multibot {tag} update"],
        "windows_x64": {
            "asset_name": asset_name,
            "url": f"{base_url}/download/{tag}/{asset_name}",
            "sha256": honest_zip_sha,
            "size_bytes": len(zip_bytes),
            "multibot_exe_sha256": exe_sha,
            "multibotd_linux_x86_64_sha256": daemon_sha,
        },
    }

    return ReleaseBundle(
        version=clean_ver,
        tag=tag,
        zip_bytes=zip_bytes,
        zip_sha256=honest_zip_sha,
        multibot_exe_sha256=exe_sha,
        multibotd_sha256=daemon_sha,
        sha256sums_txt=sha256sums,
        manifest_json=manifest,
    )


class MockReleaseServer:
    """Thread-safe local GitHub Releases API + asset server on 127.0.0.1:0."""

    def __init__(
        self,
        releases: Optional[List[MockReleaseSpec]] = None,
        *,
        rate_limit_status: Optional[int] = None,
        retry_after: Optional[str] = None,
        ratelimit_reset: Optional[str] = None,
    ) -> None:
        self.releases: List[MockReleaseSpec] = (
            list(releases) if releases is not None else [MockReleaseSpec()]
        )
        self.rate_limit_status = rate_limit_status
        self.retry_after = retry_after
        self.ratelimit_reset = ratelimit_reset
        self.requests: List[Dict[str, Any]] = []
        self._lock = threading.Lock()
        self._ready = threading.Event()
        self._httpd: Optional[ThreadingHTTPServer] = None
        self._thread: Optional[threading.Thread] = None
        self._bundles: Dict[str, ReleaseBundle] = {}

    @property
    def port(self) -> int:
        assert self._httpd is not None
        return int(self._httpd.server_address[1])

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    @property
    def latest_url(self) -> str:
        return f"{self.base_url}/repos/valentinyeo/multibot/releases/latest"

    @property
    def releases_url(self) -> str:
        return f"{self.base_url}/repos/valentinyeo/multibot/releases"

    def set_releases(self, releases: List[MockReleaseSpec]) -> None:
        with self._lock:
            self.releases = list(releases)
            self._bundles.clear()

    def set_rate_limit(
        self,
        *,
        status: Optional[int],
        retry_after: Optional[str] = None,
        ratelimit_reset: Optional[str] = None,
    ) -> None:
        with self._lock:
            self.rate_limit_status = status
            self.retry_after = retry_after
            self.ratelimit_reset = ratelimit_reset

    def get_bundle(self, spec: MockReleaseSpec) -> ReleaseBundle:
        key = f"{spec.effective_tag}:{spec.tamper_mode}"
        with self._lock:
            cached = self._bundles.get(key)
            if cached is not None:
                return cached
        bundle = build_release_bundle(
            spec.version,
            base_url=self.base_url,
            notes=spec.notes,
            published_at=spec.published_at,
            tamper_mode=spec.tamper_mode,
        )
        with self._lock:
            self._bundles[key] = bundle
        return bundle

    def _release_json(self, spec: MockReleaseSpec) -> Dict[str, Any]:
        bundle = self.get_bundle(spec)
        tag = spec.effective_tag
        assets = [
            {
                "name": "multibot-windows-x64.zip",
                "browser_download_url": f"{self.base_url}/download/{tag}/multibot-windows-x64.zip",
                "size": len(bundle.zip_bytes),
            },
            {
                "name": "SHA256SUMS.txt",
                "browser_download_url": f"{self.base_url}/download/{tag}/SHA256SUMS.txt",
                "size": len(bundle.sha256sums_txt.encode("utf-8")),
            },
            {
                "name": "update-manifest.json",
                "browser_download_url": f"{self.base_url}/download/{tag}/update-manifest.json",
                "size": len(json.dumps(bundle.manifest_json).encode("utf-8")),
            },
        ]
        return {
            "tag_name": tag,
            "name": f"Multibot {tag}",
            "draft": spec.draft,
            "prerelease": spec.prerelease,
            "published_at": spec.published_at,
            "body": "\n".join(f"- {n}" for n in spec.notes),
            "assets": assets,
        }

    def start(self) -> "MockReleaseServer":
        server_ref = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, format: str, *args: Any) -> None:
                return  # keep test output clean

            def _record(self, status: int) -> None:
                with server_ref._lock:
                    server_ref.requests.append(
                        {
                            "method": self.command,
                            "path": self.path,
                            "if_none_match": self.headers.get("If-None-Match"),
                            "status": status,
                        }
                    )

            def _send_bytes(
                self,
                status: int,
                body: bytes,
                content_type: str = "application/json",
                extra_headers: Optional[Dict[str, str]] = None,
            ) -> None:
                self._record(status)
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(body)))
                if extra_headers:
                    for k, v in extra_headers.items():
                        self.send_header(k, v)
                self.end_headers()
                if self.command != "HEAD" and body:
                    self.wfile.write(body)

            def do_GET(self) -> None:
                with server_ref._lock:
                    rl_status = server_ref.rate_limit_status
                    retry_after = server_ref.retry_after
                    rl_reset = server_ref.ratelimit_reset
                    releases = list(server_ref.releases)

                path_only = self.path.split("?", 1)[0]

                if rl_status is not None and "/releases" in path_only:
                    hdrs: Dict[str, str] = {"X-RateLimit-Remaining": "0"}
                    if retry_after is not None:
                        hdrs["Retry-After"] = retry_after
                    if rl_reset is not None:
                        hdrs["X-RateLimit-Reset"] = rl_reset
                    payload = json.dumps({"message": "API rate limit exceeded"}).encode("utf-8")
                    self._send_bytes(rl_status, payload, extra_headers=hdrs)
                    return

                if path_only.endswith("/releases/latest"):
                    if not releases:
                        self._send_bytes(404, b'{"message":"Not Found"}')
                        return
                    latest = releases[0]
                    etag = latest.effective_etag
                    if self.headers.get("If-None-Match") == etag:
                        self._record(304)
                        self.send_response(304)
                        self.send_header("ETag", etag)
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                        return
                    body = json.dumps(server_ref._release_json(latest)).encode("utf-8")
                    self._send_bytes(200, body, extra_headers={"ETag": etag})
                    return

                if path_only.endswith("/releases"):
                    etag = releases[0].effective_etag if releases else '"empty"'
                    if self.headers.get("If-None-Match") == etag:
                        self._record(304)
                        self.send_response(304)
                        self.send_header("ETag", etag)
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                        return
                    body = json.dumps([server_ref._release_json(r) for r in releases]).encode("utf-8")
                    self._send_bytes(200, body, extra_headers={"ETag": etag})
                    return

                if path_only.startswith("/download/"):
                    parts = path_only.split("/")
                    if len(parts) >= 4:
                        tag, asset = parts[2], parts[3]
                        matched = next((r for r in releases if r.effective_tag == tag), None)
                        if matched is None:
                            self._send_bytes(404, b'{"message":"tag not found"}')
                            return
                        if asset == "multibot-windows-x64.zip" and matched.tamper_mode == "evil_redirect":
                            self._record(302)
                            self.send_response(302)
                            self.send_header("Location", "http://evil.example.invalid/malware.zip")
                            self.send_header("Content-Length", "0")
                            self.end_headers()
                            return
                        if asset == "multibot-windows-x64.zip" and matched.tamper_mode == "oversized_content_length":
                            self._record(200)
                            self.close_connection = True
                            self.send_response(200)
                            self.send_header("Content-Type", "application/zip")
                            # Advertise 300 MiB (> 256 MiB cap) and close immediately so HTTP/1.1 clients never hang
                            self.send_header("Content-Length", str(300 * 1024 * 1024))
                            self.send_header("Connection", "close")
                            self.end_headers()
                            return
                        bundle = server_ref.get_bundle(matched)
                        if asset == "multibot-windows-x64.zip":
                            self._send_bytes(200, bundle.zip_bytes, "application/zip")
                            return
                        if asset == "SHA256SUMS.txt":
                            self._send_bytes(
                                200,
                                bundle.sha256sums_txt.encode("utf-8"),
                                "text/plain; charset=utf-8",
                            )
                            return
                        if asset == "update-manifest.json":
                            self._send_bytes(
                                200,
                                json.dumps(bundle.manifest_json, indent=2).encode("utf-8"),
                                "application/json",
                            )
                            return

                self._send_bytes(404, b'{"message":"not found"}')

        self._httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._httpd.daemon_threads = True

        def _serve() -> None:
            self._ready.set()
            assert self._httpd is not None
            self._httpd.serve_forever(poll_interval=0.05)

        self._thread = threading.Thread(target=_serve, name="mock-release-server", daemon=True)
        self._thread.start()
        self._ready.wait(timeout=5.0)
        return self

    def stop(self) -> None:
        if self._httpd is not None:
            self._httpd.shutdown()
            self._httpd.server_close()
            self._httpd = None
        if self._thread is not None and self._thread.is_alive():
            self._thread.join(timeout=2.0)
            self._thread = None

    def __enter__(self) -> "MockReleaseServer":
        return self.start()

    def __exit__(self, exc_type, exc, tb) -> None:
        self.stop()
