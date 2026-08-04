"""Tests for copy-on-select.

No region of the TUI could be copied: Textual implements selection but binds
nothing to action_copy_text, so a selection was made and then dropped. The app
now copies on the TextSelected event Textual posts at every mouse release.

Delivery goes to two places on purpose. Textual's copy_to_clipboard emits OSC 52
and nothing more, which many terminals ignore; the external helper is what makes
the text pastable into other applications.
"""

from __future__ import annotations

import os
import threading
import time

import pytest

from ma_app.tui import clipboard
from ma_app.tui.clipboard import ClipboardApp, copy_to_os_clipboard


class FakeScreen:
    def __init__(self, selected: str | None) -> None:
        self._selected = selected

    def get_selected_text(self) -> str | None:
        return self._selected


class Recorder:
    """Stands in for the App: records what the handler tried to copy."""

    def __init__(self, selected: str | None) -> None:
        self.screen = FakeScreen(selected)
        self.osc52: list[str] = []

    def copy_to_clipboard(self, text: str) -> None:
        self.osc52.append(text)


@pytest.fixture
def os_copies(monkeypatch) -> list[str]:
    """
    Collect what the OS helper was asked to copy.

    The handler dispatches to a worker thread — spawning the helper takes ~64 ms
    and this runs on the event loop — so `_select` joins the threads it started
    before the assertions read this list.
    """
    calls: list[str] = []
    monkeypatch.setattr(
        clipboard, "copy_to_os_clipboard", lambda text: calls.append(text) or True
    )
    return calls


def _select(app) -> None:
    before = set(threading.enumerate())
    ClipboardApp.on_text_selected(app)  # type: ignore[arg-type]
    for thread in set(threading.enumerate()) - before:
        thread.join(timeout=5)


def test_the_os_copy_does_not_block_the_event_loop(monkeypatch):
    """
    The handler must return immediately. Spawning the helper inline stalled the
    UI for the length of every drag-selection.
    """
    released = threading.Event()

    def slow_copy(text: str) -> bool:
        released.wait(timeout=5)
        return True

    monkeypatch.setattr(clipboard, "copy_to_os_clipboard", slow_copy)

    app = Recorder("some text")
    started = time.monotonic()
    ClipboardApp.on_text_selected(app)  # type: ignore[arg-type]
    elapsed = time.monotonic() - started
    released.set()

    assert elapsed < 0.5, f"handler blocked for {elapsed:.2f}s"
    assert app.osc52 == ["some text"], "OSC 52 still happens synchronously"


def test_a_selection_is_copied_to_both_destinations(os_copies):
    app = Recorder("Cmd+Shift+G")
    _select(app)
    assert app.osc52 == ["Cmd+Shift+G"]
    assert os_copies == ["Cmd+Shift+G"]


def test_an_empty_selection_copies_nothing(os_copies):
    """
    TextSelected fires on every mouse release, including a plain click that
    selected nothing. Copying "" then would wipe a clipboard filled elsewhere.
    """
    app = Recorder("")
    _select(app)
    assert app.osc52 == []
    assert os_copies == []


def test_no_selection_at_all_copies_nothing(os_copies):
    app = Recorder(None)
    _select(app)
    assert app.osc52 == []
    assert os_copies == []


def test_multiline_selections_are_preserved(os_copies):
    text = "### Step 5 — Press: Cmd+Down\n\nOpen the newly created folder."
    app = Recorder(text)
    _select(app)
    assert os_copies == [text]


def test_a_screen_that_cannot_report_a_selection_is_survivable(os_copies):
    """The screen stack may be unwinding; a copy is never worth a crash."""

    class Broken(Recorder):
        @property
        def screen(self):  # type: ignore[override]
            raise RuntimeError("no active screen")

        @screen.setter
        def screen(self, value) -> None:
            pass

    app = Broken("something")
    _select(app)
    assert app.osc52 == []
    assert os_copies == []


# The OS helper itself


def test_the_helper_receives_the_text_on_stdin(monkeypatch):
    captured = {}

    def fake_run(argv, input=None, **kwargs):
        captured["argv"] = argv
        captured["input"] = input
        return None

    monkeypatch.setattr(clipboard.shutil, "which", lambda name: f"/usr/bin/{name}")
    monkeypatch.setattr(clipboard.subprocess, "run", fake_run)
    monkeypatch.setattr(clipboard.platform, "system", lambda: "Linux")

    assert copy_to_os_clipboard("hello") is True
    assert captured["input"] == b"hello"
    assert captured["argv"][0].endswith("wl-copy")


def test_wayland_helper_is_preferred_then_x11(monkeypatch):
    """wl-copy is the only one that reaches a Wayland compositor's clipboard."""
    monkeypatch.setattr(clipboard.platform, "system", lambda: "Linux")
    monkeypatch.setattr(clipboard.subprocess, "run", lambda *a, **k: None)

    present = {"xclip"}
    monkeypatch.setattr(
        clipboard.shutil,
        "which",
        lambda name: f"/usr/bin/{name}" if name in present else None,
    )
    argv = clipboard._os_clipboard_command()
    assert argv is not None and argv[0].endswith("xclip")
    assert "-selection" in argv and "clipboard" in argv

    present.add("wl-copy")
    argv = clipboard._os_clipboard_command()
    assert argv is not None and argv[0].endswith("wl-copy")


def test_no_helper_installed_is_reported_not_raised(monkeypatch):
    monkeypatch.setattr(clipboard.platform, "system", lambda: "Linux")
    monkeypatch.setattr(clipboard.shutil, "which", lambda name: None)
    assert copy_to_os_clipboard("text") is False


def test_a_failing_helper_is_reported_not_raised(monkeypatch):
    monkeypatch.setattr(clipboard.platform, "system", lambda: "Linux")
    monkeypatch.setattr(clipboard.shutil, "which", lambda name: "/usr/bin/wl-copy")

    def boom(*args, **kwargs):
        raise OSError("helper exploded")

    monkeypatch.setattr(clipboard.subprocess, "run", boom)
    assert copy_to_os_clipboard("text") is False


def test_empty_text_never_spawns_a_helper(monkeypatch):
    def fail(*args, **kwargs):
        raise AssertionError("must not spawn a helper for empty text")

    monkeypatch.setattr(clipboard.shutil, "which", fail)
    assert copy_to_os_clipboard("") is False


@pytest.mark.skipif(os.name == "nt", reason="POSIX helper lookup")
def test_macos_uses_pbcopy(monkeypatch):
    monkeypatch.setattr(clipboard.platform, "system", lambda: "Darwin")
    monkeypatch.setattr(
        clipboard.shutil, "which", lambda name: "/usr/bin/pbcopy" if name == "pbcopy" else None
    )
    argv = clipboard._os_clipboard_command()
    assert argv == ["/usr/bin/pbcopy"]
