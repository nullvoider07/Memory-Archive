"""Tests for ImageReview's external image viewer lifecycle.

Focus: opening a step's image must REPLACE the viewer opened previously rather
than stack another fullscreen window over it. feh binds Escape to quit but quits
only the focused instance, so stacked windows read to the annotator as Escape
doing nothing — they close the top one and an identical image is still there.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from ma_app.tui.widgets import image_review
from ma_app.tui.widgets.image_review import ImageReview


class FakeProc:
    """Stand-in for subprocess.Popen: a display is not available under pytest."""

    def __init__(self, argv: list[str]) -> None:
        self.argv = argv
        self.terminated = False
        self.killed = False
        self.waited = False
        self._returncode: int | None = None

    def poll(self) -> int | None:
        return self._returncode

    def terminate(self) -> None:
        self.terminated = True
        self._returncode = -15

    def kill(self) -> None:
        self.killed = True
        self._returncode = -9

    def wait(self, timeout: float | None = None) -> int:
        self.waited = True
        return self._returncode if self._returncode is not None else 0

    def exit_on_its_own(self, code: int = 0) -> None:
        """Simulate the annotator closing the viewer with Escape."""
        self._returncode = code


@pytest.fixture
def spawned(monkeypatch: pytest.MonkeyPatch) -> list[FakeProc]:
    """Capture every viewer spawned, in order."""
    procs: list[FakeProc] = []

    def fake_popen(argv, *args, **kwargs):  # noqa: ANN001, ANN202
        p = FakeProc(list(argv))
        procs.append(p)
        return p

    monkeypatch.setattr(image_review.subprocess, "Popen", fake_popen)
    return procs


def _pane(tmp_path: Path) -> ImageReview:
    """An ImageReview with a step's frames loaded, without a running Textual app."""
    pane = ImageReview(memory_dir=tmp_path)
    frame = tmp_path / "step_0001_at.webp"
    frame.write_bytes(b"not-a-real-image")
    pane._frame_paths = [frame]
    pane._path = frame
    return pane


def test_reopening_replaces_the_previous_viewer(
    tmp_path: Path, spawned: list[FakeProc], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(image_review, "_HAS_FEH", True)
    pane = _pane(tmp_path)

    pane._open_image()
    pane._open_image()

    assert len(spawned) == 2, "each open should launch a viewer"
    assert spawned[0].terminated, "the first viewer must be closed, not left stacked"
    assert spawned[0].waited, "terminate must be followed by wait, which reaps the child"
    assert not spawned[1].terminated, "the current viewer must stay open"
    assert pane._viewer is spawned[1]


def test_a_viewer_the_annotator_already_closed_is_not_terminated(
    tmp_path: Path, spawned: list[FakeProc], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(image_review, "_HAS_FEH", True)
    pane = _pane(tmp_path)

    pane._open_image()
    spawned[0].exit_on_its_own()
    pane._open_image()

    assert not spawned[0].terminated, "must not signal a process that already exited"
    assert len(spawned) == 2
    assert pane._viewer is spawned[1]


def test_unmount_closes_a_live_viewer(
    tmp_path: Path, spawned: list[FakeProc], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(image_review, "_HAS_FEH", True)
    pane = _pane(tmp_path)

    pane._open_image()
    pane.on_unmount()

    assert spawned[0].terminated, "quitting the TUI must not leave a viewer running"
    assert pane._viewer is None


def test_kill_is_the_fallback_when_terminate_is_ignored(
    tmp_path: Path, spawned: list[FakeProc], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(image_review, "_HAS_FEH", True)
    pane = _pane(tmp_path)
    pane._open_image()

    stubborn = spawned[0]

    def terminate_without_exiting() -> None:
        stubborn.terminated = True  # signal delivered, process does not die

    def wait_times_out(timeout: float | None = None) -> int:
        raise subprocess.TimeoutExpired(cmd="feh", timeout=timeout or 0)

    monkeypatch.setattr(stubborn, "terminate", terminate_without_exiting)
    monkeypatch.setattr(stubborn, "wait", wait_times_out)

    pane._close_viewer()

    assert stubborn.killed, "a viewer that ignores SIGTERM must be killed"
    assert pane._viewer is None


def test_the_macos_launcher_is_not_tracked(
    tmp_path: Path, spawned: list[FakeProc], monkeypatch: pytest.MonkeyPatch
) -> None:
    """`open` hands off to Preview and exits; its handle is not the window."""
    monkeypatch.setattr(image_review, "_HAS_FEH", False)
    monkeypatch.setattr(image_review, "_HAS_OPEN", True)
    pane = _pane(tmp_path)

    pane._open_image()
    pane._open_image()

    assert len(spawned) == 2
    assert pane._viewer is None, "the macOS launcher must not be tracked"
    assert not any(p.terminated for p in spawned), "nothing should be terminated"
