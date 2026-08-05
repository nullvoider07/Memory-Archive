"""Tests for the compile editor's finalize key.

Ctrl+D did not finalize. Textual dispatches a key to the focused widget's
bindings before the screen's, and the compile editor is a TextArea, whose own
`delete,ctrl+d` binding consumed it — so the key deleted a character and the
finalize dialog never opened. Nothing reported the shadowing, which is why it
survived a release: a binding that loses a race looks exactly like a binding
that was never pressed.

These tests drive the real CompilerApp, because the defect only exists once a
TextArea holds focus.
"""

from __future__ import annotations

import asyncio

import pytest
from textual.widgets import Button, Input, TextArea

from ma_app.tui.app import CompilerApp
from ma_app.tui.screens.annotation import AnnotationScreen
from ma_app.tui.screens.compiler import CompilerFinalizeOverlay, CompilerScreen

DRAFT = "# Overview\n\nMoved Sample.txt off the Desktop.\n"


def _run(scenario):
    return asyncio.run(scenario())


@pytest.fixture
def memory_path(tmp_path):
    path = tmp_path / "memory.md"
    path.write_text(DRAFT, encoding="utf-8")
    return path


def test_ctrl_f_opens_the_finalize_dialog_from_inside_the_editor(memory_path):
    """The editor holds focus in normal use; the binding must outrank it."""

    async def scenario():
        app = CompilerApp("test-session", memory_path)
        async with app.run_test(size=(100, 30)) as pilot:
            await pilot.pause()
            app.screen.query_one("#memory-editor", TextArea).focus()
            await pilot.pause()
            await pilot.press("ctrl+f")
            await pilot.pause()
            return type(app.screen).__name__

    assert _run(scenario) == CompilerFinalizeOverlay.__name__


def test_the_finalize_key_does_not_edit_the_document(memory_path):
    """
    The old Ctrl+D deleted the character right of the cursor. A finalize key
    that silently mutates the draft is worse than one that does nothing.
    """

    async def scenario():
        app = CompilerApp("test-session", memory_path)
        async with app.run_test(size=(100, 30)) as pilot:
            await pilot.pause()
            editor = app.screen.query_one("#memory-editor", TextArea)
            editor.focus()
            editor.move_cursor((0, 0))
            await pilot.pause()
            await pilot.press("ctrl+f")
            await pilot.pause()
            return editor.text

    assert _run(scenario) == DRAFT


def test_ctrl_f_while_the_dialog_is_open_does_not_stack_a_second_one(memory_path):
    """
    Priority bindings resolve against the top screen's chain. Putting the
    priority copy on the App instead of the screen would leave it reachable
    with the overlay open, pushing dialog after dialog.
    """

    async def scenario():
        app = CompilerApp("test-session", memory_path)
        async with app.run_test(size=(100, 30)) as pilot:
            await pilot.pause()
            app.screen.query_one("#memory-editor", TextArea).focus()
            await pilot.pause()
            await pilot.press("ctrl+f")
            await pilot.pause()
            await pilot.press("ctrl+f")
            await pilot.pause()
            return sum(
                isinstance(screen, CompilerFinalizeOverlay) for screen in app.screen_stack
            )

    assert _run(scenario) == 1


def test_the_dialog_confirms_before_finalizing(memory_path):
    """Finalizing locks the session, so it must never be one keystroke."""

    async def scenario():
        app = CompilerApp("test-session", memory_path)
        async with app.run_test(size=(100, 30)) as pilot:
            await pilot.pause()
            app.screen.query_one("#memory-editor", TextArea).focus()
            await pilot.pause()
            await pilot.press("ctrl+f")
            await pilot.pause()
            overlay = app.screen
            focused = overlay.focused
            assert app.return_value is None, "finalized without confirmation"
            assert focused is not None, "the dialog opened with nothing focused"
            return type(focused).__name__, focused.id

    kind, button_id = _run(scenario)
    assert kind == Button.__name__
    assert button_id == "btn-cancel", "focus must start on the non-destructive button"


def _binding_keys(binding) -> set[str]:
    """BINDINGS entries may be Binding objects or plain tuples."""
    key = binding.key if hasattr(binding, "key") else binding[0]
    return {k.strip() for k in key.split(",")}


def _is_priority(binding) -> bool:
    return bool(getattr(binding, "priority", False))


@pytest.mark.parametrize(
    "screen_cls",
    [CompilerScreen, AnnotationScreen],
    ids=["compiler", "annotation"],
)
def test_no_screen_binding_is_shadowed_by_a_focused_editor(screen_cls):
    """
    The class of bug, not just the instance.

    Textual resolves a key against the focused widget's bindings before the
    screen's. Both panes that matter put a TextArea or Input in focus during
    normal use, so any non-priority screen binding sharing a key with those
    widgets can never fire — and nothing anywhere reports it. Ctrl+D reached a
    release this way, deleting a character from the draft on every press.
    """
    editor_keys = {
        key
        for widget in (TextArea, Input)
        for binding in widget.BINDINGS
        for key in _binding_keys(binding)
    }

    shadowed = sorted(
        key
        for binding in screen_cls.BINDINGS
        if not _is_priority(binding)
        for key in _binding_keys(binding) & editor_keys
    )
    assert not shadowed, (
        f"{screen_cls.__name__} bindings claimed by a focused editor, so they "
        f"will never fire: {shadowed}. Mark them priority=True or rebind them."
    )
