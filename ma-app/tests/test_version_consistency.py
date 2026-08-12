"""The version is declared in five places; they must agree.

Editing all of them is not the same as them agreeing, and two of the five are
`except ImportError` fallbacks that only take effect when `ma_app` cannot be
imported — precisely the situation where a stale number would go unnoticed,
because the code path that reports it is the one nobody exercises.

The Rust workspace is included because `ma-core` and `ma_app` ship as one
release: a `memory-archive` CLI reporting one version while the daemon beside it
reports another is a support problem that surfaces as an unreproducible bug.
"""
from __future__ import annotations

import re
import tomllib
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]

# The fallback literal inside `except ImportError:` in each module that has one.
FALLBACK = re.compile(r'^\s+__version__ = "([^"]+)"', re.MULTILINE)


def _pyproject_version() -> str:
    data = tomllib.loads((REPO / "ma-app" / "pyproject.toml").read_text())
    return data["project"]["version"]


def _cargo_version() -> str:
    data = tomllib.loads((REPO / "Cargo.toml").read_text())
    return data["workspace"]["package"]["version"]


def _package_version() -> str:
    text = (REPO / "ma-app" / "ma_app" / "__init__.py").read_text()
    match = re.search(r'^__version__ = "([^"]+)"', text, re.MULTILINE)
    assert match, "ma_app/__init__.py declares no __version__"
    return match.group(1)


@pytest.mark.parametrize("module", ["cli.py", "updater.py"])
def test_import_fallback_matches_the_package_version(module: str) -> None:
    """The fallback is dead code until an import fails, then it is the only truth."""
    text = (REPO / "ma-app" / "ma_app" / module).read_text()
    match = FALLBACK.search(text)
    assert match, f"{module} has no `except ImportError` version fallback"
    assert match.group(1) == _package_version(), (
        f"{module}'s fallback version {match.group(1)!r} has drifted from "
        f"ma_app.__version__ {_package_version()!r}. It is only reachable when "
        f"the import fails, so nothing else would catch this."
    )


def test_python_packaging_matches_the_package() -> None:
    assert _pyproject_version() == _package_version()


def test_rust_workspace_matches_the_python_package() -> None:
    """ma-core and ma_app ship together and must report the same version."""
    assert _cargo_version() == _package_version()
