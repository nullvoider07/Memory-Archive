"""Fixtures and skip rules for the Control-Center compatibility tests.

These tests drive real binaries, so they skip rather than fail when the pieces
are not staged — a developer without the Control-Center releases downloaded
still gets a clean unit-test run.
"""
from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

from harness import cc_server_binary, ma_core_binary, reset_registry


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
