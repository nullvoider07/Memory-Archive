# /Memory-Archive/ma-app/ma_app/tui/widgets/button_nav.py

from __future__ import annotations

from typing import ClassVar, Sequence

from textual import events
from textual.content import Content
from textual.css.query import NoMatches
from textual.screen import ModalScreen
from textual.widgets import Button


def hint_label(text: str, key: str) -> Content:
    """
    Build a button label whose bracketed key hint survives rendering.

    Button labels are parsed as content markup, so a literal "Quit  [q]" has
    "[q]" consumed as an unknown style tag and renders as "Quit" — the only
    documented way out of the dialog, invisible. Constructing the label as
    Content bypasses the markup parser, which is why the hint is a separate
    argument rather than something each caller has to remember to escape.
    """
    return Content(f"{text}  [{key}]")


class ButtonNavModal(ModalScreen):
    """
    Modal base adding Left/Right focus movement across its buttons.

    Textual moves focus with Tab/Shift+Tab only, so a confirm dialog is
    unnavigable with the arrow keys most users reach for. Subclasses list their
    button ids in BUTTON_ORDER to get arrow navigation that wraps at both ends,
    and name INITIAL_FOCUS to set the button focused on mount.

    INITIAL_FOCUS must name the non-destructive choice: Enter activates the
    focused button, so the default must never be the one that discards work.
    Subclasses must also make focus visually unmistakable — a coloured `variant`
    fill on the destructive button reads as "selected" and points the user at
    the opposite of what Enter will do.
    """

    BUTTON_ORDER: ClassVar[Sequence[str]] = ()
    INITIAL_FOCUS: ClassVar[str] = ""

    def on_mount(self) -> None:
        target = self.INITIAL_FOCUS or (
            self.BUTTON_ORDER[0] if self.BUTTON_ORDER else ""
        )
        if not target:
            return
        try:
            self.query_one(f"#{target}", Button).focus()
        except NoMatches:
            pass

    def on_key(self, event: events.Key) -> None:
        if event.key not in ("left", "right"):
            return
        order = list(self.BUTTON_ORDER)
        if not order:
            return
        focused_id = getattr(self.focused, "id", None)
        if focused_id not in order:
            return
        step = 1 if event.key == "right" else -1
        target = order[(order.index(focused_id) + step) % len(order)]
        try:
            self.query_one(f"#{target}", Button).focus()
        except NoMatches:
            return
        event.stop()
