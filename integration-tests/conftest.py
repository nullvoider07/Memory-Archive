"""Fixtures and skip rules for the Control-Center compatibility tests.

These tests drive real binaries, so they skip rather than fail when the pieces
are not staged — a developer without the Control-Center releases downloaded
still gets a clean unit-test run.

That leniency is wrong in CI, where every piece is installed on purpose and a
missing one is a broken workflow, not a missing convenience. Set
MA_INTEGRATION_STRICT=1 and an unmet prerequisite fails the run instead of
skipping it. Without that, this suite reports success having executed nothing:
the release gate passed green with all 14 tests skipped because the job never
installed redis-tools, so `redis-cli` was absent and every test was marked skip.

Strict mode also compares what staged against what staging discovered. A green
matrix that has quietly stopped covering a release is the same failure wearing a
different hat, and it does not announce itself in the exit code.
"""
from __future__ import annotations

import os
import re
import shutil
import subprocess
from pathlib import Path

import pytest

from harness import CC_BIN_DIR, cc_server_binary, ma_core_binary, reset_registry


def strict_mode() -> bool:
    """True when an unmet prerequisite must fail rather than skip."""
    return os.environ.get("MA_INTEGRATION_STRICT", "") not in ("", "0", "false")


def _discovered_versions() -> list[str] | None:
    """Versions the staging script last set out to stage; None if it never ran."""
    manifest = CC_BIN_DIR / "DISCOVERED"
    if not manifest.is_file():
        return None
    return [line.strip() for line in manifest.read_text().splitlines() if line.strip()]


def _redis_available() -> bool:
    if shutil.which("redis-cli") is None:
        return False
    result = subprocess.run(["redis-cli", "ping"], capture_output=True, text=True)
    return "PONG" in result.stdout


def pytest_collection_modifyitems(config, items):
    reasons = []
    if not _redis_available():
        reasons.append("redis is not reachable (redis-cli missing or not answering)")
    if ma_core_binary() is None:
        reasons.append("ma-core is not built (cargo build -p ma-core)")
    if shutil.which("memory-archive") is None:
        reasons.append("the memory-archive CLI is not on PATH")

    if reasons:
        summary = "; ".join(reasons)
        if strict_mode():
            raise pytest.UsageError(
                f"MA_INTEGRATION_STRICT is set but the environment is incomplete: "
                f"{summary}. Skipping here would report a green compatibility "
                f"matrix that ran no tests."
            )
        skip = pytest.mark.skip(reason=summary)
        for item in items:
            item.add_marker(skip)

    if not strict_mode():
        return

    # The matrix parametrises over what is on disk (see `staged_versions`), and
    # stage-cc-releases.sh treats a failed download as a warning. Composed, those
    # two reasonable choices silently shrink the matrix: a row disappears and the
    # run still exits 0. Worse, `LATEST` is the last staged version, so when the
    # newest release is the one that failed to download, the provenance test
    # retargets itself at the previous release and passes. Compare what staged
    # against what staging discovered.
    discovered = _discovered_versions()
    if discovered is None:
        raise pytest.UsageError(
            f"MA_INTEGRATION_STRICT is set but there is no staging manifest at "
            f"{CC_BIN_DIR / 'DISCOVERED'}. Run integration-tests/stage-cc-releases.sh "
            f"first — the matrix derives its rows from what is staged, so a run "
            f"without staging reports a green matrix having covered nothing."
        )
    missing = [v for v in discovered if cc_server_binary(v) is None]
    if missing:
        raise pytest.UsageError(
            f"MA_INTEGRATION_STRICT is set but staging is incomplete: "
            f"{', '.join(missing)} discovered as released but not staged. "
            f"Continuing would drop those rows — and retarget the provenance test "
            f"at an older release — while still reporting green."
        )


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
        message = f"Control-Center {version} not staged — see integration-tests/README.md"
        if strict_mode():
            # Staging is a workflow step in CI; a version missing there means the
            # download failed, which must not read as a passing matrix.
            pytest.fail(message, pytrace=False)
        pytest.skip(message)
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
