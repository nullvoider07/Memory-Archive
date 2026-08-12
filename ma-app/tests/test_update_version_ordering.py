"""The update check must not order versions semantically.

This project's version history is **not monotonic**. It ran to v0.13.2 (July
2026) and was then renumbered down to v0.2.0, so every current release sorts
*below* five older tags under semantic-version ordering:

    0.13.2  >  0.4.0        # true as semver, false as release order

`memory-archive update` is correct today because it asks GitHub for
`/releases/latest` — a pointer maintained by publication, not by version
arithmetic — and compares it to the installed version with **string equality**.
It never asks which of two versions is greater, so the discontinuity cannot
mislead it.

Replacing that equality with a semver comparison looks like an obvious
improvement and would silently break every install: a user on 0.4.0 would be told
0.13.2 is newer and rolled backwards onto a build predating the version gate, the
registry-TTL fix and the updater hotfix. This test exists to fail if that change
is ever made.
"""
from __future__ import annotations

import inspect
import re

from ma_app import updater


def _update_source() -> str:
    return inspect.getsource(updater._update_command)


def test_the_check_is_equality_not_ordering() -> None:
    """An ordering comparison against the released version is the failure mode."""
    source = _update_source()

    assert "latest_version == __version__" in source, (
        "The update check no longer compares by equality. If this was replaced "
        "with a semantic-version comparison, read this module's docstring: this "
        "project's releases are not monotonic and ordering would roll users back."
    )

    ordering = re.findall(r"latest_version\s*[<>]=?\s*", source)
    assert not ordering, (
        f"The update check orders versions ({ordering}). This project ran to "
        f"0.13.2 and was renumbered to 0.2.0, so 0.13.2 > 0.4.0 as semver while "
        f"being five releases older. Compare against GitHub's /releases/latest "
        f"pointer by equality instead."
    )


def test_the_release_pointer_is_what_is_consulted() -> None:
    """The GitHub endpoint is the source of 'newest', not a computed maximum."""
    assert "releases/latest" in updater.RELEASES_API, (
        "The updater must resolve the newest release from GitHub's own pointer. "
        "Enumerating releases and taking the maximum would reintroduce ordering."
    )
