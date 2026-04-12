# /Memory-Archive/ma-app/ma_app/tui/screens/compiler.py

from __future__ import annotations

from datetime import datetime
from pathlib import Path
from typing import ClassVar

from textual.app import ComposeResult
from textual.binding import Binding, BindingType
from textual.screen import ModalScreen, Screen
from textual.widgets import Button, Label, TextArea
from textual.css.query import NoMatches
from textual.widget import Widget


class CompilerStatusBar(Widget):
    """Bottom status bar — word count, save flash, key hints."""

    DEFAULT_CSS = """
    CompilerStatusBar {
        width: 1fr;
        height: 1;
        background: #1a1d27;
        color: #64748b;
        padding: 0 1;
        dock: bottom;
        layout: horizontal;
    }
    #compiler-status-left  { width: 1fr; height: 1; content-align: left middle; }
    #compiler-status-right { width: auto; height: 1; content-align: right middle; }
    """

    def compose(self) -> ComposeResult:
        yield Label("", id="compiler-status-left")
        yield Label(
            "[#5865f2]Ctrl+S[/#5865f2] Save  "
            "[#5865f2]Ctrl+Q[/#5865f2] Quit",
            id="compiler-status-right",
        )

    def update(self, word_count: int, saved_at: str | None, unsaved: bool) -> None:
        try:
            left = self.query_one("#compiler-status-left", Label)
        except NoMatches:
            return

        parts: list[str] = [f"{word_count} words"]
        if saved_at and not unsaved:
            parts.append(f"[#22c55e]✓ Saved {saved_at}[/#22c55e]")
        elif unsaved:
            parts.append("[#fbbf24]● unsaved[/#fbbf24]")
        left.update("  ".join(parts))

    def flash_saved(self, saved_at: str) -> None:
        try:
            left = self.query_one("#compiler-status-left", Label)
        except NoMatches:
            return
        left.update(f"[#22c55e]✓ Saved {saved_at}[/#22c55e]")
        self.set_timer(0.8, lambda: None)


class CompilerQuitOverlay(ModalScreen):
    """Unsaved changes on quit. Dismissed with 'save', 'discard', or None (cancel)."""

    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("escape", "cancel", show=False),
        Binding("s",      "save",   show=False),
        Binding("d",      "discard", show=False),
    ]

    DEFAULT_CSS = """
    CompilerQuitOverlay { align: center middle; }
    #cq-box {
        width: 52; height: auto;
        background: #1a1d27; border: round #f59e0b; padding: 1 2;
    }
    #cq-title {
        width: 1fr; content-align: center middle;
        text-style: bold; color: #f59e0b; margin-bottom: 1;
    }
    #cq-body  { width: 1fr; color: #e2e8f0; margin-bottom: 1; }
    #cq-buttons {
        width: 1fr; height: auto;
        layout: horizontal; align: center middle;
    }
    #cq-buttons Button { margin: 0 1; }
    """

    def compose(self) -> ComposeResult:
        with Widget(id="cq-box"):
            yield Label("Unsaved edits", id="cq-title")
            yield Label(
                "memory.md has unsaved changes.\n"
                "Save before closing?",
                id="cq-body",
            )
            with Widget(id="cq-buttons"):
                yield Button("Save & exit  [s]",  id="btn-save",    variant="success")
                yield Button("Discard  [d]",       id="btn-discard", variant="error")
                yield Button("Cancel  [Esc]",      id="btn-cancel",  variant="primary")

    def on_mount(self) -> None:
        try:
            self.query_one("#btn-save", Button).focus()
        except NoMatches:
            pass

    def on_button_pressed(self, event: Button.Pressed) -> None:
        mapping = {
            "btn-save":    "save",
            "btn-discard": "discard",
            "btn-cancel":  None,
        }
        self.dismiss(mapping.get(event.button.id or "", None))

    def action_save(self)    -> None: self.dismiss("save")
    def action_discard(self) -> None: self.dismiss("discard")
    def action_cancel(self)  -> None: self.dismiss("discard")


class CompilerScreen(Screen):
    """
    Full-screen memory.md editor for T4.3.

    Pre-populated with the scaffolded draft from T4.2.
    Ctrl+S atomically saves to disk.
    Ctrl+Q prompts if unsaved changes, then exits with result='complete'.
    """

    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("ctrl+s", "save",     "Save",  show=False),
        Binding("ctrl+q", "quit_editor", "Quit", show=False),
    ]

    DEFAULT_CSS = """
    CompilerScreen {
        background: #0f1117;
        layout: vertical;
    }
    #compiler-title {
        width: 1fr; height: 1;
        background: #1a1d27;
        color: #e2e8f0;
        padding: 0 1;
        text-style: bold;
        dock: top;
    }
    #memory-editor {
        width: 1fr;
        height: 1fr;
        border: none;
    }
    """

    def __init__(self, memory_path: Path, initial_text: str, session_id: str = "") -> None:
        super().__init__()
        self._memory_path = memory_path
        self._initial_text = initial_text
        self._session_id = session_id
        self._last_saved_at: str | None = None
        self._saved_text = initial_text

    def compose(self) -> ComposeResult:
        yield Label(f"  {self._memory_path.name}", id="compiler-title")
        yield TextArea(self._initial_text, id="memory-editor", soft_wrap=True)
        yield CompilerStatusBar(id="compiler-status-bar")

    def on_mount(self) -> None:
        try:
            editor = self.query_one("#memory-editor", TextArea)
            editor.focus()
            editor.theme = "vscode_dark"
        except NoMatches:
            pass
        self._refresh_status()
        self.set_interval(2.5, self._autosave)

    def _autosave(self) -> None:
        try:
            current = self.query_one("#memory-editor", TextArea).text
        except NoMatches:
            return
        if current.rstrip() != self._saved_text.rstrip():
            self._do_save()

    def on_text_area_changed(self, _event: TextArea.Changed) -> None:
        self._refresh_status()

    def action_save(self) -> None:
        self._do_save()

    def action_quit_editor(self) -> None:
        try:
            current = self.query_one("#memory-editor", TextArea).text
        except NoMatches:
            self.app.exit(result="complete")
            return
        if current.rstrip() != self._saved_text.rstrip():
            self.app.push_screen(CompilerQuitOverlay(), self._on_quit_choice)
        else:
            self.app.exit(result="complete")

    def _on_quit_choice(self, choice: str | None = None) -> None:
        if choice == "save":
            self._do_save()
            self.app.exit(result="complete")
        elif choice == "discard":
            self.app.exit(result="complete")
        # choice == None means Cancel — do nothing, user stays in editor

    def _do_save(self) -> None:
        try:
            editor = self.query_one("#memory-editor", TextArea)
        except NoMatches:
            return

        text = editor.text
        tmp  = self._memory_path.with_suffix(".md.tmp")
        try:
            tmp.write_text(text, encoding="utf-8")
            tmp.rename(self._memory_path)
        except OSError:
            return

        self._last_saved_at = datetime.now().strftime("%H:%M:%S")
        self._saved_text = text
        from ma_app.storage.sync_worker import get_worker, FileWrittenEvent
        worker = get_worker()
        if worker is not None:
            worker.enqueue(FileWrittenEvent(
                session_id=self._session_id,
                relative_path="memory.md",
                abs_path=str(self._memory_path),
            ))
        self._refresh_status()

    def _refresh_status(self) -> None:
        try:
            editor = self.query_one("#memory-editor", TextArea)
            bar    = self.query_one("#compiler-status-bar", CompilerStatusBar)
        except NoMatches:
            return

        word_count = len(editor.text.split())
        has_unsaved = editor.text != self._saved_text
        bar.update(word_count, self._last_saved_at, has_unsaved)