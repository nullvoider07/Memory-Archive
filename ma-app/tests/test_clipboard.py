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

from textual.widgets.text_area import Selection

from ma_app.tui import clipboard
from ma_app.tui.clipboard import (
    ClipboardApp,
    copy_to_os_clipboard,
    paste_from_os_clipboard,
)
from ma_app.tui.screens.compiler import CompilerScreen
from ma_app.tui.session_loader import StepState
from ma_app.tui.widgets.reasoning_editor import ReasoningEditor

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


# Paste replaces the selection


class EditorApp(ClipboardApp):
    """The reasoning editor on its own, so Ctrl+V goes through its binding."""

    def compose(self) -> ComposeResult:
        yield ReasoningEditor()


def _editor_paste(initial: str, pasted: str, select, monkeypatch) -> tuple[str, tuple]:
    """
    Drive one Ctrl+V through the real widget and return (text, cursor).

    Both editors route through clipboard.paste_into, which reads the OS
    clipboard via the module global patched here.
    """
    monkeypatch.setattr(clipboard, "paste_from_os_clipboard", lambda: pasted)

    async def scenario():
        app = EditorApp()
        async with app.run_test(size=(60, 20)) as pilot:
            area = app.query_one("#editor-area", TextArea)
            area.load_text(initial)
            area.focus()
            await pilot.pause()
            select(area)
            await pilot.pause()
            await pilot.press("ctrl+v")
            await pilot.pause()
            return area.text, area.cursor_location

    return _run(scenario)


def test_paste_over_select_all_replaces_the_document(monkeypatch):
    """
    The reported bug: Ctrl+A then Ctrl+V appended instead of replacing.

    action_paste called TextArea.insert(), which writes at cursor_location and
    leaves the selection alone. After select-all the cursor sits at the end of
    the document, so the pasted text landed immediately after the text it was
    supposed to overwrite, with no separator.
    """
    text, cursor = _editor_paste(
        "old reasoning text",
        "new reasoning text",
        lambda area: area.select_all(),
        monkeypatch,
    )
    assert text == "new reasoning text"
    assert cursor == (0, len("new reasoning text")), "cursor must follow the paste"


def test_paste_over_a_partial_selection_replaces_only_it(monkeypatch):
    text, _ = _editor_paste(
        "alpha beta gamma",
        "BETA",
        lambda area: setattr(area, "selection", Selection((0, 6), (0, 10))),
        monkeypatch,
    )
    assert text == "alpha BETA gamma"


def test_paste_over_a_backwards_selection_replaces_it(monkeypatch):
    """A drag from right to left gives a Selection whose end precedes its start."""
    text, _ = _editor_paste(
        "alpha beta gamma",
        "BETA",
        lambda area: setattr(area, "selection", Selection((0, 10), (0, 6))),
        monkeypatch,
    )
    assert text == "alpha BETA gamma"


def test_paste_with_no_selection_still_inserts_at_the_cursor(monkeypatch):
    """The collapsed case must keep working — replace over an empty range."""
    text, cursor = _editor_paste(
        "alpha gamma",
        "beta ",
        lambda area: area.move_cursor((0, 6)),
        monkeypatch,
    )
    assert text == "alpha beta gamma"
    assert cursor == (0, 11)


def test_cut_from_the_outer_widget_reaches_the_os_clipboard(os_copies):
    """
    Tab focuses the ReasoningEditor itself, not its TextArea.

    Ctrl+A/C/V were bound on the outer widget for exactly that case but Ctrl+X
    was not, so cut silently did nothing whenever the editor had been reached by
    Tab rather than by clicking into the text.
    """

    async def scenario():
        app = EditorApp()
        async with app.run_test(size=(60, 20)) as pilot:
            editor = app.query_one(ReasoningEditor)
            area = app.query_one("#editor-area", TextArea)
            area.load_text("cut this line")
            area.selection = Selection((0, 0), (0, 8))
            editor.focus()
            await pilot.pause()
            await pilot.press("ctrl+x")
            await pilot.pause()
            return area.text

    remaining = _run(scenario)
    assert os_copies == ["cut this"]
    assert remaining == " line"


# The compile editor


class CompilerApp(ClipboardApp):
    """The memory.md editor, driven the way `memory-archive compile` runs it."""

    def __init__(self, path, text: str) -> None:
        super().__init__()
        self._compiler = CompilerScreen(path, text)

    def get_default_screen(self) -> CompilerScreen:
        # The compile screen must be the *default* screen, not one pushed from
        # on_mount: a push is queued and the editor would not exist yet when the
        # test starts driving keys at it.
        return self._compiler


def _compile_editor(tmp_path, initial: str, body):
    async def scenario():
        app = CompilerApp(tmp_path / "memory.md", initial)
        async with app.run_test(size=(80, 24)) as pilot:
            await pilot.pause()
            area = app.query_one("#memory-editor", TextArea)
            await body(app, pilot, area)
            return area.text

    return _run(scenario)


def test_compile_editor_pastes_from_the_os_clipboard(tmp_path, monkeypatch):
    """
    Ctrl+V here fell through to TextArea.action_paste, which reads App.clipboard
    and nothing else — so text copied in any other application had nowhere to
    land in the file that is the whole point of the compile step.
    """
    monkeypatch.setattr(clipboard, "paste_from_os_clipboard", lambda: "## Overview")

    async def body(app, pilot, area):
        area.move_cursor(area.document.end)
        await pilot.press("ctrl+v")
        await pilot.pause()

    assert _compile_editor(tmp_path, "# Memory: x\n", body) == "# Memory: x\n## Overview"


def test_compile_editor_paste_replaces_the_selection(tmp_path, monkeypatch):
    monkeypatch.setattr(clipboard, "paste_from_os_clipboard", lambda: "replacement")

    async def body(app, pilot, area):
        area.select_all()
        await pilot.press("ctrl+v")
        await pilot.pause()

    assert _compile_editor(tmp_path, "the old draft", body) == "replacement"


def test_compile_editor_ctrl_a_selects_all(tmp_path):
    """
    TextArea binds ctrl+a to cursor_line_start, so the same keystroke meant
    select-all in the reasoning editor and "go to column 0" here.
    """
    captured: list[str] = []

    async def body(app, pilot, area):
        await pilot.press("ctrl+a")
        await pilot.pause()
        captured.append(area.selected_text)

    _compile_editor(tmp_path, "line one\nline two", body)
    assert captured == ["line one\nline two"]


# The dirty check


def _dirty_after_save(buffer_text: str) -> bool:
    """
    Save `buffer_text` the way AnnotationScreen does and report if it stays dirty.

    The screen posts StepSaved with text.strip() and feeds that same stripped
    string back through mark_saved, so the baseline is stripped while the buffer
    is not.
    """
    step = StepState(
        step_id=1,
        timestamp="t",
        action_type="mouse",
        action_subtype="left",
        image_path=None,
        image_fetched=True,
        marked=True,
    )

    async def scenario():
        app = EditorApp()
        async with app.run_test(size=(60, 20)) as pilot:
            editor = app.query_one(ReasoningEditor)
            editor.enter_edit_mode(step)
            await pilot.pause()
            app.query_one("#editor-area", TextArea).load_text(buffer_text)
            await pilot.pause()
            editor.mark_saved(1, buffer_text.strip())
            return editor.has_unsaved_draft()

    return _run(scenario)


def test_a_trailing_newline_does_not_leave_the_step_permanently_dirty():
    """
    Pressing Enter at the end of an annotation made the editor dirty forever:
    autosave rewrote reasoning.jsonl every 2.5 s and quitting always claimed
    unsaved changes, because the raw buffer was compared to a stripped baseline.
    """
    assert _dirty_after_save("some reasoning\n") is False
    assert _dirty_after_save("  leading and trailing  ") is False


def test_a_real_edit_is_still_reported_as_unsaved():
    """The stripped comparison must not swallow an actual change."""
    assert _dirty_after_save("saved text") is False

    step = StepState(
        step_id=1,
        timestamp="t",
        action_type="mouse",
        action_subtype="left",
        image_path=None,
        image_fetched=True,
        marked=True,
    )

    async def scenario():
        app = EditorApp()
        async with app.run_test(size=(60, 20)) as pilot:
            editor = app.query_one(ReasoningEditor)
            editor.enter_edit_mode(step)
            await pilot.pause()
            area = app.query_one("#editor-area", TextArea)
            area.load_text("saved text")
            await pilot.pause()
            editor.mark_saved(1, "saved text")
            area.load_text("saved text, now edited")
            await pilot.pause()
            return editor.has_unsaved_draft()

    assert _run(scenario) is True


def test_paste_falls_back_to_the_in_app_clipboard(monkeypatch):
    """With no OS helper the widget still pastes what was copied in the TUI."""
    monkeypatch.setattr(clipboard, "paste_from_os_clipboard", lambda: "")

    async def scenario():
        app = EditorApp()
        async with app.run_test(size=(60, 20)) as pilot:
            area = app.query_one("#editor-area", TextArea)
            area.load_text("old text")
            area.focus()
            await pilot.pause()
            app.copy_to_clipboard("in-app text")
            area.select_all()
            await pilot.pause()
            await pilot.press("ctrl+v")
            await pilot.pause()
            return area.text

    assert _run(scenario) == "in-app text"
