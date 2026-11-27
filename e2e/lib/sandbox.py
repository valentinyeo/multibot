"""Isolated per-test sandbox environment and read-only integrity helpers."""

from __future__ import annotations

import hashlib
import os
import shutil
import stat
import tempfile
import time
from pathlib import Path
from typing import Dict, Optional, Tuple

from . import FIXTURES_DIR

TreeSnapshot = Dict[str, Tuple[int, int, str]]

STRIPPED_ENV_VARS = (
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "MULTIBOT_DATA_DIR",
    "MULTIBOT_UPDATE_URL",
    "MULTIBOT_GPU_MODE",
    "MULTIBOT_GPU_LUID",
    "MULTIBOT_GPU_REPORT_PATH",
    "MULTIBOT_BUILD_VERSION",
    "MULTIBOT_CONFIG_DIR",
)


def snapshot_tree(directory: Path) -> TreeSnapshot:
    """Capture deterministic (size, mtime_ns, sha256) snapshot of all files under directory."""
    snap: TreeSnapshot = {}
    if not directory.exists():
        return snap
    for path in sorted(directory.rglob("*")):
        rel = path.relative_to(directory).as_posix()
        if path.is_file():
            st = path.stat()
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            snap[rel] = (st.st_size, st.st_mtime_ns, digest)
        elif path.is_dir():
            snap[rel + "/"] = (0, 0, "dir")
    return snap


def restore_write_permissions(root: Path) -> None:
    """Ensure every non-symlink directory and file under root is owner-writable before deletion."""
    if root.is_symlink() or not root.exists():
        return
    try:
        os.chmod(root, stat.S_IRWXU)
    except OSError:
        pass
    for dirpath, dirnames, filenames in os.walk(root, topdown=True, followlinks=False):
        dp = Path(dirpath)
        if not dp.is_symlink():
            try:
                os.chmod(dp, stat.S_IRWXU)
            except OSError:
                pass
        for d in dirnames:
            target = dp / d
            if not target.is_symlink():
                try:
                    os.chmod(target, stat.S_IRWXU)
                except OSError:
                    pass
        for f in filenames:
            target = dp / f
            if not target.is_symlink():
                try:
                    os.chmod(target, stat.S_IRUSR | stat.S_IWUSR)
                except OSError:
                    pass


class IsolatedHome:
    """Context manager providing an isolated HOME/XDG/APPDATA/TMPDIR sandbox."""

    def __init__(self, prefix: str = "multibot_e2e_") -> None:
        self.root = Path(tempfile.mkdtemp(prefix=prefix))
        self.home = self.root / "home"
        self.config_dir = self.home / ".config"
        self.data_dir = self.home / ".local" / "share"
        self.cache_dir = self.home / ".cache"
        self.appdata_dir = self.config_dir
        self.localappdata_dir = self.home / ".local" / "appdata"
        self.tmp_dir = self.root / "tmp"
        self.fake_bin_dir = self.root / "fake-bin"

        self.claude_projects_dir = self.home / ".claude" / "projects"
        self.codex_sessions_dir = self.home / ".codex" / "sessions"
        self.agy_brain_dir = self.home / ".gemini" / "antigravity" / "brain"
        self.multibot_dir = self.home / ".multibot"
        self.multibot_history_dir = self.multibot_dir / "history"
        self.multibot_config_dir = self.config_dir / "Multibot"
        self.zigshel_dir = self.config_dir / "ZigShell"

        for d in (
            self.home,
            self.config_dir,
            self.data_dir,
            self.cache_dir,
            self.localappdata_dir,
            self.tmp_dir,
            self.fake_bin_dir,
        ):
            d.mkdir(parents=True, exist_ok=True)

        self._cleaned = False

    def __enter__(self) -> "IsolatedHome":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.cleanup()

    def stage_fixture(
        self,
        src_rel: str,
        dst_rel: str,
        *,
        mtime_offset_sec: Optional[float] = None,
    ) -> Path:
        """Copy a fixture file from e2e/fixtures/<src_rel> into <home>/<dst_rel>."""
        src = FIXTURES_DIR / src_rel
        if not src.exists():
            raise FileNotFoundError(f"Fixture not found: {src}")
        dst = self.home / dst_rel
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dst)
        if mtime_offset_sec is not None:
            target = time.time() + mtime_offset_sec
            os.utime(dst, (target, target))
        return dst

    def stage_text(
        self,
        dst_rel: str,
        content: str,
        *,
        mtime_offset_sec: Optional[float] = None,
    ) -> Path:
        """Write deterministic text content into <home>/<dst_rel>."""
        dst = self.home / dst_rel
        dst.parent.mkdir(parents=True, exist_ok=True)
        dst.write_text(content, encoding="utf-8")
        if mtime_offset_sec is not None:
            target = time.time() + mtime_offset_sec
            os.utime(dst, (target, target))
        return dst

    def stage_zigshel_fixtures(self, *, readonly: bool = True) -> TreeSnapshot:
        """Stage legacy ZigShell fixtures into .config/ZigShell and optionally chmod read-only."""
        self.zigshel_dir.mkdir(parents=True, exist_ok=True)
        src_dir = FIXTURES_DIR / "zigshel"
        for item in sorted(src_dir.iterdir()):
            if item.is_file():
                shutil.copy2(item, self.zigshel_dir / item.name)
        if readonly:
            return self.lock_readonly(self.zigshel_dir)
        return snapshot_tree(self.zigshel_dir)

    def install_fake_cli(self, name: str, script_body: str) -> Path:
        """Install an executable fake CLI shim into fake_bin_dir."""
        target = self.fake_bin_dir / name
        target.write_text(script_body, encoding="utf-8")
        os.chmod(target, 0o755)
        return target

    def lock_readonly(self, directory: Path) -> TreeSnapshot:
        """Set files to 0444 and directories to 0555 and return SHA-256/mtime snapshot."""
        snap = snapshot_tree(directory)
        for path in sorted(directory.rglob("*"), reverse=True):
            if path.is_symlink():
                continue
            if path.is_file():
                os.chmod(path, 0o444)
            elif path.is_dir():
                os.chmod(path, 0o555)
        if not directory.is_symlink() and directory.is_dir():
            os.chmod(directory, 0o555)
        return snap

    def assert_unchanged(self, directory: Path, expected: TreeSnapshot) -> None:
        """Assert that directory contents, file sizes, mtime_ns, and SHA-256 hashes are identical."""
        actual = snapshot_tree(directory)
        if actual != expected:
            expected_keys = set(expected.keys())
            actual_keys = set(actual.keys())
            added = sorted(actual_keys - expected_keys)
            removed = sorted(expected_keys - actual_keys)
            modified = sorted(
                k for k in (expected_keys & actual_keys) if expected[k] != actual[k]
            )
            raise AssertionError(
                f"Read-only directory modified ({directory}): "
                f"added={added}, removed={removed}, modified={modified}"
            )

    def build_env(
        self,
        *,
        use_fake_bin: bool = False,
        extra_env: Optional[Dict[str, str]] = None,
    ) -> Dict[str, str]:
        """Construct a clean child environment isolated from host Multibot/Claude/Codex state."""
        env = dict(os.environ)
        for key in list(env.keys()):
            if key in STRIPPED_ENV_VARS or key.startswith("MULTIBOT_"):
                env.pop(key, None)

        env["HOME"] = str(self.home)
        env["XDG_CONFIG_HOME"] = str(self.config_dir)
        env["XDG_DATA_HOME"] = str(self.data_dir)
        env["XDG_CACHE_HOME"] = str(self.cache_dir)
        env["APPDATA"] = str(self.appdata_dir)
        env["LOCALAPPDATA"] = str(self.localappdata_dir)
        env["TMPDIR"] = str(self.tmp_dir)

        if use_fake_bin:
            base_path = env.get("PATH", "/usr/bin:/bin")
            env["PATH"] = f"{self.fake_bin_dir}:{base_path}"

        if extra_env:
            env.update(extra_env)
        return env

    def cleanup(self) -> None:
        """Restore write permissions and recursively delete the temp sandbox."""
        if self._cleaned:
            return
        self._cleaned = True
        try:
            restore_write_permissions(self.root)
            shutil.rmtree(self.root, ignore_errors=False)
        except Exception:
            restore_write_permissions(self.root)
            shutil.rmtree(self.root, ignore_errors=True)
