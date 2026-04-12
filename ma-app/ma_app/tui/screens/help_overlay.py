# /Memory-Archive/ma-app/ma_app/tui/screens/help_overlay.py

from __future__ import annotations

from typing import ClassVar

from textual.app import ComposeResult
from textual.binding import Binding, BindingType
from textual.screen import ModalScreen
from textual.widget import Widget
from textual.widgets import Label


# All shortcuts shown in the help overlay, grouped by section.
# Format: (key_label, description) — empty string = section header.
_SECTIONS: list[tuple[str, str]] = [
    # Navigation
    ("NAVIGATION",              ""),
    ("↑  /  k",                 "Previous step"),
    ("↓  /  j",                 "Next step"),
    ("PgUp  /  PgDn",           "Fast scroll"),
    ("Ctrl+Shift+E",            "Jump to step by number"),
    # Editing
    ("EDITING",                 ""),
    ("Enter  /  e",             "Edit selected step"),
    ("Ctrl+S",                  "Save reasoning (stay on step)"),
    ("Ctrl+N",                  "Save + advance to next step"),
    ("Ctrl+N  (2×, empty)",     "Skip step"),
    ("Ctrl+Z  /  Ctrl+Y",       "Undo / Redo"),
    ("u",                       "Revert to last saved (when list focused)"),
    ("Space",                   "Expand / collapse step dropdown"),
    # Image pane
    ("IMAGE",                   ""),
    ("+  /  =",                 "Zoom in"),
    ("-",                       "Zoom out"),
    ("f",                       "Fit image to pane"),
    ("Scroll",                  "Zoom (mouse wheel)"),
    ("Click + drag",            "Pan (requires Pillow)"),
    # UI
    ("UI",                      ""),
    ("Tab",                     "Cycle pane focus"),
    ("?",                       "Toggle this help overlay"),
    ("Ctrl+Q",                  "Quit"),
]


class HelpOverlay(ModalScreen):
    """
    Full-screen help overlay listing all keyboard shortcuts.

    Pushed by AnnotationScreen.action_show_help().
    Dismissed by Escape, ?, or q.
    """

    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("escape",        "dismiss_overlay", show=False),
        Binding("question_mark", "dismiss_overlay", show=False),
        Binding("q",             "dismiss_overlay", show=False),
    ]

    DEFAULT_CSS = """
    HelpOverlay {
        align: center middle;
    }

    #help-box {
        width: 62;
        height: auto;
        max-height: 90%;
        background: $surface;
        border: round $primary;
        padding: 1 2;
    }

    #help-title {
        width: 1fr;
        content-align: center middle;
        text-style: bold;
        color: $accent;
        margin-bottom: 1;
    }

    .section-header {
        height: 1;
        color: $text-muted;
        text-style: bold;
        margin-top: 1;
        padding-left: 1;
    }

    .help-row {
        height: 1;
        layout: horizontal;
    }

    .help-key {
        width: 26;
        color: $accent;
        text-style: bold;
        padding-left: 2;
    }

    .help-desc {
        width: 1fr;
        color: $text;
    }

    #help-footer {
        width: 1fr;
        content-align: center middle;
        color: $text-muted;
        margin-top: 1;
    }
    """

    def compose(self) -> ComposeResult:
        with Widget(id="help-box"):
            yield Label("⌨  Keyboard Shortcuts", id="help-title")

            for key_label, desc in _SECTIONS:
                if desc == "":
                    # Section header (desc is empty string, key_label is the title).
                    yield Label(f"── {key_label} ──", classes="section-header")
                else:
                    with Widget(classes="help-row"):
                        yield Label(key_label, classes="help-key")
                        yield Label(desc,      classes="help-desc")

            yield Label("Esc  ·  ?  ·  q  to close", id="help-footer")

    def action_dismiss_overlay(self) -> None:
        self.dismiss()