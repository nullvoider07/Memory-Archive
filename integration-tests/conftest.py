"""Fixtures and skip rules for the Control-Center compatibility tests.

These tests drive real binaries, so they skip rather than fail when the pieces
are not staged — a developer without the Control-Center releases downloaded
still gets a clean unit-test run.
"""
from __future__ import annotations

import re
import shutil
import subprocess
from pathlib import Path

import pytest

from harness import CC_BIN_DIR, cc_server_binary, ma_core_binary, reset_registry


def _redis_available() -> bool:
    if shutil.which("redis-cli") is None:
        return False
    result = subprocess.run(["redis-cli", "ping"], capture_output=True, text=True)
    return "PONG" in result.stdout


def pytest_collection_modifyitems(config, items):
    reasons = []
    if not _redis_available():
        reasons.append("redis is not reachable")
    if ma_core_binary() is None:
        reasons.append("ma-core is not built (cargo build -p ma-core)")
    if shutil.which("memory-archive") is None:
        reasons.append("the memory-archive CLI is not on PATH")

    if reasons:
        skip = pytest.mark.skip(reason="; ".join(reasons))
        for item in items:
            item.add_marker(skip)


@pytest.fixture()
def clean_registry():
    """Flush the test Redis DB either side of a test. Never touches DB 0."""
    reset_registry()
    yield
    reset_registry()


@pytest.fixture()
def workdir(tmp_path: Path) -> Path:
    return tmp_path


def require_cc(version: str) -> Path:
    binary = cc_server_binary(version)
    if binary is None:
        pytest.skip(f"Control-Center {version} not staged — see integration-tests/README.md")
    return binary


def _as_tuple(version: str) -> tuple[int, ...]:
    return tuple(int(part) for part in version.split("."))


def staged_versions(minimum: str | None = None) -> list[str]:
    """Control-Center versions present in the staging directory, oldest first.

    The matrix is derived from what is staged rather than written down, so a new
    Control-Center release is covered by re-running stage-cc-releases.sh — no test
    edit. A hard-coded list silently stops covering the newest release, which is
    the case these tests exist to catch.

    Returns a single placeholder when nothing is staged so collection still yields
    a test, which then skips through `require_cc` with an actionable message.
    """
    if not CC_BIN_DIR.is_dir():
        return [minimum or "0.0.0"]

    found = sorted(
        (d.name for d in CC_BIN_DIR.iterdir()
         if d.is_dir() and (d / "control-center-server").is_file()
         and re.fullmatch(r"\d+\.\d+\.\d+", d.name)),
        key=_as_tuple,
    )
    if minimum:
        found = [v for v in found if _as_tuple(v) >= _as_tuple(minimum)]
    return found or [minimum or "0.0.0"]
