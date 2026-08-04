"""Tests for confirm-dialog button labels and arrow navigation.

Two defects are covered here, both reported from a live compile session:

  * The bracketed key hints were silently eaten. Button labels are parsed as
    content markup, so "Quit  [q]" rendered as "Quit" — hiding the only key
    that closed the dialog.
  * Left/Right did nothing, because Textual moves focus with Tab/Shift+Tab and
    the overlays bound only Escape and a letter.

The dialogs are driven headless through Textual's own pilot rather than by
poking at compose(), so what is asserted is what a user would see and press.
asyncio.run drives the async pilot from a sync test — the suite has no
pytest-asyncio dependency and CI installs none.
"""

from __future__ import annotations

import asyncio
from typing import Any, Awaitable, Callable

import pytest
from textual.app import App
from textual.widgets import Button

from ma_app.tui.screens.annotation import (
    AnnotationCompleteOverlay,
    CrashRecoveryOverlay,
    QuitConfirmOverlay,
)
from ma_app.tui.screens.compiler import (
    CompilerFinalizeOverlay,
    CompilerQuitOverlay,
)
from ma_app.tui.widgets.button_nav import ButtonNavModal, hint_label


def _make(overlay_cls: type) -> Any:
    """Build an overlay, supplying the constructor args the class needs."""
    if overlay_cls is AnnotationCompleteOverlay:
        return overlay_cls(annotated=3, skipped=0)
    if overlay_cls is CrashRecoveryOverlay:
        return overlay_cls(7)
    return overlay_cls()


def _drive(overlay_cls: type, body: Callable[[Any, Any], Awaitable[None]]) -> None:
    """Mount one overlay headless and run `body(app, pilot)` against it."""

    class Host(App):
        def on_mount(self) -> None:
            self.push_screen(_make(overlay_cls))

    async def _run() -> None:
        app = Host()
        async with app.run_test() as pilot:
            await pilot.pause()
            await body(app, pilot)

    asyncio.run(_run())


def _labels(app: App) -> list[str]:
    return [button.label.plain for button in app.screen.query(Button)]


def _focused_id(app: App) -> str | None:
    return getattr(app.screen.focused, "id", None)


# Labels — the markup bug


def test_hint_label_keeps_the_bracketed_key():
    """
    Content bypasses the markup parser. Passing the same string through
    from_markup is what dropped "[q]" as an unknown tag.
    """
    from textual.content import Content

    assert hint_label("Quit", "q").plain == "Quit  [q]"
    assert Content.from_markup("Quit  [q]").plain == "Quit  "


@pytest.mark.parametrize(
    "overlay_cls, expected",
    [
        (CompilerQuitOverlay,     ["Quit  [q]", "Cancel  [Esc]"]),
        (CompilerFinalizeOverlay, ["Finalize  [f]", "Cancel  [Esc]"]),
        (QuitConfirmOverlay,      ["Quit  [q]", "Cancel  [Esc]"]),
        (CrashRecoveryOverlay,    ["Resume step 7  [r]", "Start from step 1  [Esc]"]),
        (
            AnnotationCompleteOverlay,
            ["Compile now  [c]", "Compile later  [l]", "Quit  [q]"],
        ),
    ],
)
def test_every_dialog_button_renders_its_key_hint(overlay_cls, expected):
    """Regression guard: a hint written as a plain string renders without it."""

    async def _check(app, _pilot) -> None:
        rendered = _labels(app)
        for text in expected:
            assert text in rendered, (
                f"{overlay_cls.__name__} lost the hint {text!r}; rendered {rendered}"
            )

    _drive(overlay_cls, _check)


# Arrow navigation


@pytest.mark.parametrize(
    "overlay_cls, start, after_right, after_left",
    [
        (CompilerQuitOverlay,     "btn-cancel", "btn-quit",     "btn-quit"),
        (CompilerFinalizeOverlay, "btn-cancel", "btn-finalize", "btn-finalize"),
        (QuitConfirmOverlay,      "btn-cancel", "btn-quit",     "btn-quit"),
    ],
)
def test_arrows_move_focus_in_two_button_dialogs(
    overlay_cls, start, after_right, after_left
):
    """
    Both dialogs hold two buttons, so either arrow reaches the other one —
    that is the wrap-around working, in the smallest case there is.
    """

    async def _check(app, pilot) -> None:
        assert _focused_id(app) == start
        await pilot.press("right")
        assert _focused_id(app) == after_right
        await pilot.press("left")
        assert _focused_id(app) == start
        await pilot.press("left")
        assert _focused_id(app) == after_left

    _drive(overlay_cls, _check)


def test_arrows_walk_and_wrap_a_three_button_dialog():
    async def _check(app, pilot) -> None:
        assert _focused_id(app) == "btn-compile"
        await pilot.press("right")
        assert _focused_id(app) == "btn-later"
        await pilot.press("right")
        assert _focused_id(app) == "btn-quit"
        await pilot.press("right")
        assert _focused_id(app) == "btn-compile", "right must wrap to the start"
        await pilot.press("left")
        assert _focused_id(app) == "btn-quit", "left must wrap to the end"

    _drive(AnnotationCompleteOverlay, _check)


def test_a_key_that_is_not_an_arrow_is_left_for_the_bindings():
    """Escape and the letter shortcuts must still dismiss the dialog."""
    stub = _StubDialog("btn-a")
    event = _FakeKey("escape")
    ButtonNavModal.on_key(stub, event)  # type: ignore[arg-type]
    assert not event.stopped
    assert stub.queried == []


def test_focus_outside_the_button_row_is_ignored():
    """A focused input inside a dialog keeps its own arrow-key behaviour."""
    stub = _StubDialog("jump-input")
    event = _FakeKey("right")
    ButtonNavModal.on_key(stub, event)  # type: ignore[arg-type]
    assert not event.stopped
    assert stub.queried == []


# Default focus


@pytest.mark.parametrize(
    "overlay_cls",
    [CompilerQuitOverlay, CompilerFinalizeOverlay, QuitConfirmOverlay],
)
def test_confirm_dialogs_default_to_the_non_destructive_button(overlay_cls):
    """
    Enter activates the focused button. Where the other choice discards work or
    is irreversible, the default must never be that choice — this is the half
    of the bug that made an orange Quit look selected while Enter meant Cancel.
    """
    assert overlay_cls.INITIAL_FOCUS == "btn-cancel"
    assert "btn-cancel" in overlay_cls.BUTTON_ORDER

    async def _check(app, _pilot) -> None:
        assert _focused_id(app) == "btn-cancel"

    _drive(overlay_cls, _check)


class _FakeButton:
    def __init__(self, button_id: str) -> None:
        self.id = button_id

    def focus(self) -> None:  # pragma: no cover - never reached in these tests
        raise AssertionError("focus() must not be called in these cases")


class _StubDialog:
    """
    Duck-typed stand-in for a mounted dialog.

    Not a ButtonNavModal subclass: `focused` is a Textual reactive, and setting
    one on an instance that never ran Screen.__init__ raises. on_key is called
    unbound instead, which exercises the same code against a plain object.
    """

    BUTTON_ORDER = ("btn-a", "btn-b")

    def __init__(self, focused_id: str) -> None:
        self.focused = _FakeButton(focused_id)
        self.queried: list[str] = []

    def query_one(self, selector: str, expect_type=None) -> _FakeButton:
        self.queried.append(selector)
        return _FakeButton(selector.lstrip("#"))


class _FakeKey:
    def __init__(self, key: str) -> None:
        self.key = key
        self.stopped = False

    def stop(self) -> None:
        self.stopped = True
