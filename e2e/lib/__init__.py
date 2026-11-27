"""Multibot opaque-box E2E test library (pure Python 3.12 stdlib)."""

from pathlib import Path

E2E_DIR = Path(__file__).resolve().parent.parent
REPO_ROOT = E2E_DIR.parent
FIXTURES_DIR = E2E_DIR / "fixtures"
