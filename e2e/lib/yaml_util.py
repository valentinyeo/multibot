"""Zero-dependency YAML loader using system yq with Ruby stdlib fallback."""

from __future__ import annotations

import json
import shutil
import subprocess
from pathlib import Path
from typing import Any


def load_yaml(path: Path) -> Any:
    """Parse a YAML file into Python objects without requiring PyYAML."""
    if not path.exists():
        raise FileNotFoundError(f"YAML file not found: {path}")

    errors: list[str] = []

    yq = shutil.which("yq")
    if yq:
        proc = subprocess.run(
            [yq, ".", str(path)],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=10,
        )
        if proc.returncode == 0 and proc.stdout.strip():
            try:
                return json.loads(proc.stdout)
            except json.JSONDecodeError as exc:
                errors.append(f"yq produced non-JSON output: {exc}")
        else:
            errors.append(f"yq failed (rc={proc.returncode}): {proc.stderr.strip()}")

    ruby = shutil.which("ruby")
    if ruby:
        proc = subprocess.run(
            [
                ruby,
                "-ryaml",
                "-rjson",
                "-e",
                "puts JSON.generate(YAML.load_file(ARGV[0]))",
                str(path),
            ],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=10,
        )
        if proc.returncode == 0 and proc.stdout.strip():
            try:
                return json.loads(proc.stdout)
            except json.JSONDecodeError as exc:
                errors.append(f"ruby YAML produced invalid JSON: {exc}")
        else:
            errors.append(f"ruby failed (rc={proc.returncode}): {proc.stderr.strip()}")

    raise RuntimeError(
        f"Failed to parse YAML {path} via yq or ruby: {'; '.join(errors) or 'no parser found'}"
    )
