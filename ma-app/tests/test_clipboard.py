"""Tests for copy-on-select.

No region of the TUI could be copied. Textual asks the terminal for the mouse
(SET_ANY_EVENT_MOUSE and friends, in its driver), which stops the terminal doing
its own drag-to-select, and then implements selection in two disconnected ways:
a screen-level model for static content, and a private one inside TextArea and
Input, which capture the mouse and so never reach the screen-level path.

v0.3.4 handled only the screen-level model. It was proven against a Static in a
synthetic app — the one case that works — and shipped broken in the compile
editor and the reasoning editor, the two places copying actually matters.

So every app-level test here drives a **real** app through a **real** drag, over
each widget kind, and the two editable ones are the point of the exercise.

Delivery goes to two places on purpose. Textual's copy_to_clipboard emits OSC 52
and nothing more, which many terminals ignore; the external helper is what makes
the text pastable into other applications.
"""

from __future__ import annotations

import asyncio
import os
import threading
import time

import pytest
from textual import events
from textual.app import ComposeResult
from textual.widgets import Input, Static, TextArea

from ma_app.tui import clipboard
from ma_app.tui.clipboard import (
    ClipboardApp,
    copy_to_os_clipboard,
    paste_from_os_clipboard,
)

STATIC_TEXT = "static line one"
AREA_TEXT = "alpha beta gamma\ndelta epsilon"
INPUT_TEXT = "input text here"


class ProbeApp(ClipboardApp):
    """A real app with one widget of each selection model."""

    def compose(self) -> ComposeResult:
        yield Static(STATIC_TEXT, id="stat")
        yield TextArea(AREA_TEXT, id="area")
        yield Input(value=INPUT_TEXT, id="inp")


def _mouse(cls, widget, offset):
    """
    Build a mouse event at a content-relative offset, in screen coordinates.

    Anchored on content_region, not region: TextArea's default border and
    padding take two cells at the left and one at the top, so a region-relative
    offset lands several characters away from the one it names.
    """
    x = widget.content_region.x + offset[0]
    y = widget.content_region.y + offset[1]
    return cls(
        widget=None,
        x=x,
        y=y,
        delta_x=0,
        delta_y=0,
        button=1,
        shift=False,
        meta=False,
        ctrl=False,
        screen_x=x,
        screen_y=y,
        style=None,
    )


async def _drag(app, pilot, widget, start, end) -> None:
    """
    Press, move and release, posted through App.on_event like the real driver.

    Pilot's own mouse helpers call Screen._forward_event directly and skip
    App.on_event, so they would not exercise the app-level handlers at all.
    """
    app.post_message(_mouse(events.MouseDown, widget, start))
    await pilot.pause()
    for row in range(start[1], end[1] + 1):
        first = start[0] + 1 if row == start[1] else 0
        last = end[0] if row == end[1] else widget.content_region.width - 1
        for x in range(first, last + 1):
            app.post_message(_mouse(events.MouseMove, widget, (x, row)))
            await pilot.pause()
    app.post_message(_mouse(events.MouseUp, widget, end))
    await pilot.pause()
    await pilot.pause()


class SyncWorker:
    """Records handoffs to the OS clipboard without the background thread."""

    def __init__(self) -> None:
        self.copies: list[str] = []

    def submit(self, text: str) -> None:
        self.copies.append(text)


@pytest.fixture
def os_copies(monkeypatch) -> list[str]:
    worker = SyncWorker()
    monkeypatch.setattr(clipboard, "_worker", worker)
    return worker.copies


def _run(coro_factory):
    """Run one app coroutine; these tests do not depend on pytest-asyncio."""
    return asyncio.run(coro_factory())


# Real drags over each selection model


def test_dragging_over_static_text_copies(os_copies):
    async def scenario():
        app = ProbeApp()
        async with app.run_test(size=(60, 20)) as pilot:
            await _drag(app, pilot, app.query_one("#stat", Static), (0, 0), (10, 0))
            return app.clipboard

    osc52 = _run(scenario)
    assert os_copies == ["static line"]
    assert osc52 == "static line", "OSC 52 must be written as well"


def test_dragging_inside_a_text_area_copies(os_copies):
    """
    The regression test for v0.3.4.

    TextArea calls capture_mouse() on mouse-down, and Screen only opens a
    screen-level selection when nothing has captured the mouse — so the screen
    reports no selection here and the text lives in TextArea.selected_text.
    This is the compile editor and the reasoning editor.
    """

    async def scenario():
        app = ProbeApp()
        async with app.run_test(size=(60, 20)) as pilot:
            area = app.query_one("#area", TextArea)
            await _drag(app, pilot, area, (3, 0), (12, 0))
            assert app.screen.get_selected_text() in (None, ""), (
                "precondition: the screen-level model must be empty here, "
                "otherwise this test is not exercising the bug"
            )
            return area.selected_text

    selected = _run(scenario)
    assert selected, "the TextArea itself must have a selection"
    assert os_copies == [selected]


def test_dragging_inside_an_input_copies(os_copies):
    """Input captures the mouse for the same reason — the jump-to-step box."""

    async def scenario():
        app = ProbeApp()
        async with app.run_test(size=(60, 20)) as pilot:
            inp = app.query_one("#inp", Input)
            await _drag(app, pilot, inp, (1, 0), (8, 0))
            return inp.selected_text

    selected = _run(scenario)
    assert selected
    assert os_copies == [selected]


def test_a_drag_that_selects_nothing_copies_nothing(os_copies):
    """
    A press and release at one spot posts TextSelected with an empty selection.
    Copying "" then would wipe a clipboard the user had filled elsewhere.
    """

    async def scenario():
        app = ProbeApp()
        async with app.run_test(size=(60, 20)) as pilot:
            for selector, kind in (("#stat", Static), ("#area", TextArea), ("#inp", Input)):
                widget = app.query_one(selector, kind)
                await _drag(app, pilot, widget, (2, 0), (2, 0))

    _run(scenario)
    assert os_copies == []


def test_clicking_away_does_not_reuse_the_previous_widget_selection(os_copies):
    """
    The origin widget is remembered across a drag, so it must be re-read on the
    next mouse-down — otherwise a later click anywhere would re-copy the stale
    TextArea selection on every release.
    """

    async def scenario():
        app = ProbeApp()
        async with app.run_test(size=(60, 20)) as pilot:
            await _drag(app, pilot, app.query_one("#area", TextArea), (3, 0), (12, 0))
            os_copies.clear()
            await _drag(app, pilot, app.query_one("#stat", Static), (0, 0), (0, 0))

    _run(scenario)
    assert os_copies == []


def test_a_multiline_drag_keeps_its_line_breaks(os_copies):
    """Reasoning spans lines; a copy that flattened them would be useless."""

    async def scenario():
        app = ProbeApp()
        async with app.run_test(size=(60, 20)) as pilot:
            area = app.query_one("#area", TextArea)
            await _drag(app, pilot, area, (2, 0), (5, 1))
            return area.selected_text

    selected = _run(scenario)
    assert "\n" in selected, f"selection did not cross a line: {selected!r}"
    assert os_copies == [selected]


# Keyboard copy paths


def test_ctrl_c_in_a_text_area_reaches_the_os_clipboard(os_copies):
    """
    TextArea.action_copy calls App.copy_to_clipboard, which is OSC 52 only in
    Textual. Overriding it at app level is what routes every built-in copy —
    TextArea, Input, and both cut actions — to the system clipboard.
    """

    async def scenario():
        app = ProbeApp()
        async with app.run_test(size=(60, 20)) as pilot:
            area = app.query_one("#area", TextArea)
            area.focus()
            await pilot.pause()
            area.select_all()
            area.action_copy()
            await pilot.pause()
            return area.selected_text

    selected = _run(scenario)
    assert os_copies == [selected]


def test_ctrl_c_in_an_input_reaches_the_os_clipboard(os_copies):
    async def scenario():
        app = ProbeApp()
        async with app.run_test(size=(60, 20)) as pilot:
            inp = app.query_one("#inp", Input)
            inp.focus()
            await pilot.pause()
            inp.action_select_all()
            inp.action_copy()
            await pilot.pause()
            return inp.selected_text

    selected = _run(scenario)
    assert os_copies == [selected]


def test_copying_empty_text_never_reaches_the_helper(os_copies):
    async def scenario():
        app = ProbeApp()
        async with app.run_test(size=(60, 20)):
            app.copy_to_clipboard("")

    _run(scenario)
    assert os_copies == []


# The real screens


def test_the_compile_editor_copies_a_selection(os_copies, tmp_path):
    """
    Verified in CompilerApp itself, not a stand-in. The compile editor is a
    full-screen TextArea and is where the user found copy-on-select broken.
    """
    from ma_app.tui.app import CompilerApp

    memory_path = tmp_path / "memory.md"
    memory_path.write_text("# Overview\n\nMoved Sample.txt off the Desktop.\n", encoding="utf-8")

    async def scenario():
        app = CompilerApp("test-session", memory_path)
        async with app.run_test(size=(100, 30)) as pilot:
            # CompilerApp pushes CompilerScreen from on_mount, so the editor is
            # not on the default screen — wait for the push to settle.
            await pilot.pause()
            editor = app.screen.query_one("#memory-editor", TextArea)
            await _drag(app, pilot, editor, (2, 0), (9, 0))
            return editor.selected_text

    selected = _run(scenario)
    assert selected, "a drag in the compile editor must select something"
    assert os_copies == [selected]


def test_the_reasoning_editor_copies_a_selection(os_copies, tmp_path):
    """
    Verified in AnnotationApp itself. The reasoning editor is the other pane
    where copy-on-select was reported broken, and it wraps its TextArea in a
    custom widget, so its selection has one more layer to travel through.
    """
    from pathlib import Path

    from ma_app.tui.app import AnnotationApp
    from ma_app.tui.session_loader import SessionState, StepState, StepStatus
    from ma_app.tui.widgets.reasoning_editor import ReasoningEditor

    session = SessionState(
        session_id="test-session",
        memory_dir=Path(tmp_path),
        memory_name="test",
        mode="manual",
        total_steps=1,
        annotated_steps=0,
        skipped_steps=0,
        steps=[
            StepState(
                step_id=1,
                timestamp="2026-08-05T00:00:00Z",
                action_type="keyboard",
                action_subtype="press",
                image_path=None,
                image_fetched=False,
                marked=False,
                status=StepStatus.PENDING,
            )
        ],
    )

    async def scenario():
        app = AnnotationApp(session)
        async with app.run_test(size=(120, 40)) as pilot:
            await pilot.pause()
            editor = app.screen.query_one(ReasoningEditor)
            area = editor.query_one("#editor-area", TextArea)
            area.load_text("The Finder window was already frontmost.")
            await pilot.pause()
            await _drag(app, pilot, area, (4, 0), (10, 0))
            selected = area.selected_text
            # Skip the unmount-time session close; this test is about copying.
            app._annotation_complete = True
            return selected

    selected = _run(scenario)
    assert selected, "a drag in the reasoning editor must select something"
    assert os_copies == [selected]


# Worker semantics


def test_the_os_copy_does_not_block_the_event_loop(monkeypatch):
    """
    Handing off must return immediately. Spawning the helper inline stalled the
    UI for the length of every drag-selection (~64 ms measured).
    """
    released = threading.Event()

    def slow_copy(text: str) -> bool:
        released.wait(timeout=5)
        return True

    monkeypatch.setattr(clipboard, "copy_to_os_clipboard", slow_copy)

    worker = clipboard._ClipboardWorker()
    started = time.monotonic()
    worker.submit("some text")
    elapsed = time.monotonic() - started
    released.set()

    assert elapsed < 0.5, f"submit blocked for {elapsed:.2f}s"


def test_only_one_helper_runs_at_a_time_and_the_last_text_wins(monkeypatch):
    """
    A thread per copy let helpers finish out of order, leaving the clipboard
    holding an earlier selection than the one just made — measured at 300 rapid
    selections landing on number 150.
    """
    live = 0
    peak = 0
    guard = threading.Lock()
    written: list[str] = []
    gate = threading.Event()

    def counting_copy(text: str) -> bool:
        nonlocal live, peak
        with guard:
            live += 1
            peak = max(peak, live)
        gate.wait(timeout=5)
        written.append(text)
        with guard:
            live -= 1
        return True

    monkeypatch.setattr(clipboard, "copy_to_os_clipboard", counting_copy)

    worker = clipboard._ClipboardWorker()
    for i in range(50):
        worker.submit(f"selection {i}")
    gate.set()
    for thread in threading.enumerate():
        if thread.name == "ma-clipboard":
            thread.join(timeout=5)

    assert peak == 1, f"{peak} helpers ran concurrently"
    assert written[-1] == "selection 49", f"clipboard left holding {written[-1]!r}"


# The OS helpers


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


def test_paste_reads_the_os_clipboard(monkeypatch):
    """Ctrl+V in the reasoning editor pastes what another application copied."""
    import subprocess

    monkeypatch.setattr(clipboard.platform, "system", lambda: "Linux")
    monkeypatch.setattr(
        clipboard.shutil,
        "which",
        lambda name: "/usr/bin/wl-paste" if name == "wl-paste" else None,
    )
    monkeypatch.setattr(
        clipboard.subprocess,
        "run",
        lambda *a, **k: subprocess.CompletedProcess(a, 0, b"pasted text", b""),
    )
    assert paste_from_os_clipboard() == "pasted text"


def test_a_failing_paste_helper_yields_empty_not_an_error(monkeypatch):
    monkeypatch.setattr(clipboard.platform, "system", lambda: "Linux")
    monkeypatch.setattr(clipboard.shutil, "which", lambda name: "/usr/bin/wl-paste")

    def boom(*args, **kwargs):
        raise OSError("helper exploded")

    monkeypatch.setattr(clipboard.subprocess, "run", boom)
    assert paste_from_os_clipboard() == ""
