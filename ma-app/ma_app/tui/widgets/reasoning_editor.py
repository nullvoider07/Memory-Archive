# /Memory-Archive/ma-app/ma_app/tui/widgets/reasoning_editor.py

from __future__ import annotations

import subprocess
import shutil
from typing import ClassVar, Optional

from textual import on
from textual import events
from textual.app import ComposeResult
from textual.binding import Binding, BindingType
from textual.css.query import NoMatches
from textual.message import Message
from textual.widget import Widget
from textual.widgets import Label, TextArea

from ma_app.tui.session_loader import StepState, StepStatus


def _is_mouse_sequence_fragment(ch: str) -> bool:
    """
    Return True if ch looks like a fragment of an SGR mouse escape sequence
    that leaked past Textual's input parser (e.g. '[', '<', 'M', 'm' paired
    with digits and semicolons from sequences like ESC[<35;42;43M).
    These arrive when the outer ReasoningEditor widget holds focus instead of
    the inner TextArea, and the ESC prefix is consumed while the rest leaks.
    """
    return bool(ch) and all(c in "<>[];Mm0123456789" for c in ch)

class ReasoningEditor(Widget, can_focus=True):
    """
    Multi-line reasoning editor for the bottom-left section.

    Layout:
        ┌─ REASONING [Step N]  ·  Words: 12  Chars: 67 ──┐
        │  TextArea (editable)                            │
        │  ...                                            │
        └─────────────────────────────────────────────────┘
        (optional)  ⚠  Reasoning required — press Ctrl+N again to skip

    Keyboard (active regardless of whether TextArea or outer widget is focused):
        Ctrl+S   — save current step (posts StepSaved)
        Ctrl+N   — save + complete step (posts StepCompleted);
                   if empty: first press shows skip hint, second press posts StepSkipped
        Ctrl+Z   — TextArea native undo
        Ctrl+Y   — TextArea native redo

    The `u` keybinding (revert to last saved) is intentionally NOT handled here.
    It is bound at AnnotationScreen level so it only fires when the TextArea does
    NOT have focus (i.e. when the user is navigating the step list).

    Posts up to AnnotationScreen:
        ReasoningEditor.StepSaved(step_id, reasoning)
        ReasoningEditor.StepCompleted(step_id, reasoning)
        ReasoningEditor.StepSkipped(step_id)
    """

    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("ctrl+s", "save",  "Save",          show=False),
        Binding("ctrl+n", "next",  "Save & Next",   show=False),
        Binding("ctrl+a", "select_all", "Select All", show=False, priority=True),
        Binding("ctrl+c", "copy", "Copy", show=False, priority=True),
        Binding("ctrl+v", "paste", "Paste", show=False, priority=True),
    ]

    DEFAULT_CSS = """
    ReasoningEditor {
        width: 1fr;
        height: 1fr;
        layout: vertical;
        border: round #5865f2;
        background: #0f1117;
    }

    ReasoningEditor > #editor-header {
        width: 1fr;
        height: 1;
        background: #1a1d27;
        color: #e2e8f0;
        padding: 0 1;
    }

    ReasoningEditor > TextArea {
        width: 1fr;
        height: 1fr;
        border: round #2a2d3a;
        padding: 0 0;
        background: #0f1117;
        color: #e2e8f0;
        margin: 0 1 0 1;
    }

    ReasoningEditor > TextArea:focus {
        border: round #5865f2;
    }

    ReasoningEditor > #skip-hint {
        width: 1fr;
        height: 1;
        background: #92400e;
        color: #fbbf24;
        padding: 0 1;
        display: none;
    }

    ReasoningEditor > #skip-hint.visible {
        display: block;
    }
    """

    # Messages
    class StepSaved(Message):
        """
        Posted when Ctrl+S is pressed.
        AnnotationScreen: update StepState.reasoning in memory, flash status bar.
        T3.6: write to reasoning.jsonl.
        """
        def __init__(self, step_id: int, reasoning: str) -> None:
            super().__init__()
            self.step_id = step_id
            self.reasoning = reasoning

    class StepCompleted(Message):
        """
        Posted when Ctrl+N is pressed with non-empty reasoning.
        AnnotationScreen: mark step COMPLETE, advance to next step.
        T3.6: write to reasoning.jsonl.
        """
        def __init__(self, step_id: int, reasoning: str) -> None:
            super().__init__()
            self.step_id = step_id
            self.reasoning = reasoning

    class StepSkipped(Message):
        """
        Posted when Ctrl+N is pressed twice with empty reasoning (confirmed skip).
        AnnotationScreen: mark step SKIPPED, advance to next step.
        T3.6: write to reasoning.jsonl with skipped=true.
        """
        def __init__(self, step_id: int) -> None:
            super().__init__()
            self.step_id = step_id

    def __init__(self) -> None:
        super().__init__()

        # Step currently loaded into the editor (None until first load_step call).
        self._current_step: Optional[StepState] = None

        # Per-step saved reasoning baseline — used by 'u' revert.
        # Keyed by step_id, value = the text as it was last written to disk
        # (or as it was restored from reasoning.jsonl on session load).
        # Updated by mark_saved() which is called by AnnotationScreen after T3.6 writes.
        self._saved_text: dict[int, str] = {}

        # Per-step unsaved drafts — preserved when the user navigates away
        # mid-edit without saving, so they can come back and resume.
        self._drafts: dict[int, str] = {}

        # Skip-confirm state: set True on first Ctrl+N with empty content,
        # cleared on any content change or second Ctrl+N.
        self._skip_pending: bool = False

    # Compose
    def compose(self) -> ComposeResult:
        yield Label(
            "REASONING  ·  Words: 0  Chars: 0",
            id="editor-header",
        )
        yield TextArea("", id="editor-area", language=None)
        yield Label(
            "⚠  Reasoning required — press Ctrl+N again to skip",
            id="skip-hint",
        )

    def on_mount(self) -> None:
        self.set_interval(2.5, self._autosave)

    def _autosave(self) -> None:
        if self._current_step is None:
            return
        current = self._get_text()
        saved   = self._saved_text.get(self._current_step.step_id, "")
        if current != saved:
            self.post_message(self.StepSaved(self._current_step.step_id, current.strip()))

    # Public API (called by AnnotationScreen)
    def load_step(self, step: StepState) -> None:
        """
        Load a step into the editor without stealing focus.

        Called when the user selects a step by navigating the list (single click,
        j/k keys).  The editor shows the step's content but focus stays in the
        list unless the user explicitly enters edit mode.

        Any unsaved draft for the previously loaded step is preserved in
        self._drafts so it survives navigation back-and-forth.
        """
        self._save_current_draft()
        self._load_step_internal(step, focus=False)

    def enter_edit_mode(self, step: StepState) -> None:
        """
        Load a step and focus the TextArea.

        Called when the user presses Enter/e, double-clicks a row, or when
        AnnotationScreen advances to the next step after Ctrl+N.
        """
        self._save_current_draft()
        self._load_step_internal(step, focus=True)

    def current_step_id(self) -> Optional[int]:
        """Return the step_id currently loaded, or None if nothing is loaded."""
        return self._current_step.step_id if self._current_step else None

    def has_unsaved_draft(self) -> bool:
        """
        Return True if the TextArea contains text that differs from the last
        saved version for the current step.

        Used by AnnotationScreen.action_quit() to decide whether to show the
        unsaved-changes confirmation prompt.
        """
        if self._current_step is None:
            return False
        current = self._get_text()
        saved   = self._saved_text.get(self._current_step.step_id, "")
        return current != saved

    def mark_saved(self, step_id: int, reasoning: str) -> None:
        """
        Update the saved-text baseline for a step.

        Called by AnnotationScreen (T3.6) immediately after writing to
        reasoning.jsonl so that 'u' (revert) always reverts to the persisted
        version, not the version from the initial load.
        """
        self._saved_text[step_id] = reasoning
        # Always clear the draft when a step is saved — not just when it's
        # the currently displayed step. Autosave can fire after navigation,
        # leaving a stale draft that would override the saved reasoning on
        # return to the step.
        self._drafts.pop(step_id, None)

    def revert_current_step(self) -> None:
        """
        Revert the TextArea to the last saved reasoning for the current step.

        Called by AnnotationScreen when the 'u' binding fires (the 'u' binding
        lives at the screen level so it doesn't interfere with typing in the
        TextArea).
        """
        if self._current_step is None:
            return
        saved = self._saved_text.get(self._current_step.step_id, "")
        self._set_text(saved)
        self._update_header(saved)
        self._hide_skip_hint()
        self._skip_pending = False
        # Drop any in-flight draft for this step.
        self._drafts.pop(self._current_step.step_id, None)

    # Internal helpers
    def _save_current_draft(self) -> None:
        """Snapshot the current TextArea content into self._drafts."""
        if self._current_step is None:
            return
        text = self._get_text()
        self._drafts[self._current_step.step_id] = text

    def _load_step_internal(self, step: StepState, *, focus: bool) -> None:
        """Swap editor content to the given step."""
        self._current_step = step
        self._skip_pending = False
        self._hide_skip_hint()

        # Priority: unsaved draft > disk-loaded reasoning in StepState
        text = self._drafts.get(
            step.step_id,
            step.reasoning or "",
        )

        # Seed the saved-text baseline from StepState on first encounter
        # (reasoning.jsonl content is already in step.reasoning via SessionLoader).
        if step.step_id not in self._saved_text:
            self._saved_text[step.step_id] = step.reasoning or ""

        self._set_text(text)
        self._update_header(text)

        if focus:
            try:
                self.query_one("#editor-area", TextArea).focus()
            except NoMatches:
                pass

    def _get_text(self) -> str:
        """Return the current TextArea content, or empty string on error."""
        try:
            return self.query_one("#editor-area", TextArea).text
        except NoMatches:
            return ""

    def _set_text(self, text: str) -> None:
        """Set TextArea content, clearing undo history for this load."""
        try:
            self.query_one("#editor-area", TextArea).load_text(text)
        except NoMatches:
            pass

    def _update_header(self, text: str) -> None:
        """Refresh the word/char counter in the title bar."""
        words = len(text.split()) if text.strip() else 0
        chars = len(text)
        if self._current_step:
            step_num = f"Step {self._current_step.step_id}"
            editing  = (
                "  [cyan]Editing…[/cyan]"
                if self._current_step.status == StepStatus.IN_PROGRESS
                else ""
            )
        else:
            step_num = "No step loaded"
            editing  = ""

        try:
            self.query_one("#editor-header", Label).update(
                f"REASONING [{step_num}]{editing}  ·  Words: {words}  Chars: {chars}"
            )
        except NoMatches:
            pass

    def _show_skip_hint(self) -> None:
        try:
            self.query_one("#skip-hint").add_class("visible")
        except NoMatches:
            pass

    def _hide_skip_hint(self) -> None:
        try:
            self.query_one("#skip-hint").remove_class("visible")
        except NoMatches:
            pass

    def on_click(self, event: events.Click) -> None:
        try:
            self.query_one("#editor-area", TextArea).focus()
        except NoMatches:
            pass

    def on_key(self, event: events.Key) -> None:
        if event.character and _is_mouse_sequence_fragment(event.character):
            event.prevent_default()
            event.stop()

    # TextArea change handler
    @on(TextArea.Changed, "#editor-area")
    def on_editor_changed(self, event: TextArea.Changed) -> None:
        """Update word/char counter and cancel skip-pending on any new text."""
        text = event.text_area.text
        self._update_header(text)

        # Any edit that adds content cancels the skip-pending warning.
        if self._skip_pending and text.strip():
            self._skip_pending = False
            self._hide_skip_hint()

    # Binding actions
    def action_save(self) -> None:
        """Ctrl+S — save the current step's reasoning (no advance)."""
        if self._current_step is None:
            return

        text = self._get_text().strip()
        self._skip_pending = False
        self._hide_skip_hint()

        # Post regardless of whether text is empty — an empty save is allowed
        # (lets the user clear reasoning if they made a mistake before T3.6 confirms).
        self.post_message(self.StepSaved(self._current_step.step_id, text))

    def action_next(self) -> None:
        """
        Ctrl+N — save + complete step, or confirm skip on second empty press.

        Flow:
          text present   → post StepCompleted (save + mark COMPLETE + advance)
          text empty (1st press) → show skip hint, set _skip_pending
          text empty (2nd press) → post StepSkipped (mark SKIPPED + advance)
        """
        if self._current_step is None:
            return

        text = self._get_text().strip()

        if not text:
            if self._skip_pending:
                # Confirmed skip.
                self._skip_pending = False
                self._hide_skip_hint()
                self.post_message(self.StepSkipped(self._current_step.step_id))
            else:
                # First empty Ctrl+N — warn the user.
                self._skip_pending = True
                self._show_skip_hint()
            return

        # Has content — save and complete.
        self._skip_pending = False
        self._hide_skip_hint()
        self.post_message(self.StepCompleted(self._current_step.step_id, text))

    # Select all actions
    def action_select_all(self) -> None:
        """Ctrl+A — select all text in the reasoning editor."""
        try:
            ta = self.query_one("#editor-area", TextArea)
            ta.select_all()
        except NoMatches:
            pass
    
    # Clipboard helpers
    @staticmethod
    def _clipboard_read() -> str:
        """Read text from the system clipboard. Returns empty string on failure."""
        # Try pyperclip first (cross-platform, pip install pyperclip)
        try:
            import pyperclip  # type: ignore
            return pyperclip.paste() or ""
        except Exception:
            pass

        # Wayland-native first, then X11 fallback
        for cmd in (
            ["wl-paste", "--no-newline"],
            ["xclip", "-selection", "clipboard", "-o"],
            ["xsel", "--clipboard", "--output"],
        ):
            if shutil.which(cmd[0]):
                try:
                    r = subprocess.run(
                        cmd, capture_output=True, timeout=2,
                        stdin=subprocess.DEVNULL,
                    )
                    if r.returncode == 0:
                        return r.stdout.decode("utf-8", errors="replace")
                except Exception:
                    pass

        # macOS fallback
        if shutil.which("pbpaste"):
            try:
                r = subprocess.run(["pbpaste"], capture_output=True, timeout=2)
                if r.returncode == 0:
                    return r.stdout.decode("utf-8", errors="replace")
            except Exception:
                pass

        return ""

    @staticmethod
    def _clipboard_write(text: str) -> None:
        """Write text to the system clipboard. Silent on failure."""
        try:
            import pyperclip  # type: ignore
            pyperclip.copy(text)
            return
        except Exception:
            pass

        for cmd in (
            ["wl-copy"],
            ["xclip", "-selection", "clipboard"],
            ["xsel", "--clipboard", "--input"],
        ):
            if shutil.which(cmd[0]):
                try:
                    subprocess.run(
                        cmd, input=text.encode("utf-8"),
                        capture_output=True, timeout=2,
                        stdin=subprocess.PIPE if cmd[0] == "wl-copy" else subprocess.PIPE,
                    )
                    return
                except Exception:
                    pass

        if shutil.which("pbcopy"):
            try:
                subprocess.run(["pbcopy"], input=text.encode("utf-8"),
                               capture_output=True, timeout=2)
            except Exception:
                pass

    # Clipboard actions
    def action_copy(self) -> None:
        """Ctrl+C — copy selected text to system clipboard."""
        try:
            ta = self.query_one("#editor-area", TextArea)
            selected = ta.selected_text
            if selected:
                self._clipboard_write(selected)
        except NoMatches:
            pass

    def action_paste(self) -> None:
        """Ctrl+V — paste from system clipboard at cursor position."""
        try:
            ta = self.query_one("#editor-area", TextArea)
            text = self._clipboard_read()
            if text:
                ta.insert(text)
        except NoMatches:
            pass