# /Memory-Archive/ma-app/ma_app/tui/widgets/step_list.py

from __future__ import annotations

import re
from pathlib import Path
from typing import ClassVar
from textual import on
from textual.app import ComposeResult
from textual.binding import Binding, BindingType
from textual.css.query import NoMatches
from textual.message import Message
from textual.reactive import reactive
from textual.containers import VerticalScroll
from textual.widget import Widget
from textual.widgets import Label, Static
from ma_app.tui.session_loader import SessionState, StepState, StepStatus

# converted_input.md parser
def load_converted_titles(memory_dir: Path) -> dict[int, str]:
    """
    Parse converted_input.md and return a {step_id: title} mapping.

    Each data row has the format:
        |    1 | 2026-02-25T12:58:04.286Z | Click at (960, 540) |

    Header rows (# ..., | Step |, |----) are skipped.
    Returns an empty dict if the file does not exist.
    """
    path = memory_dir / "commands" / "converted_input.md"
    if not path.exists():
        return {}

    titles: dict[int, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        # Only process pipe-delimited data rows with at least 3 columns.
        if not line.startswith("|") or line.startswith("|---") or "Step" in line[:20]:
            continue
        cols = [c.strip() for c in line.strip("|").split("|")]
        if len(cols) < 3:
            continue
        try:
            step_id = int(cols[0])
            title = cols[2]  # Action column
            # Strip [FAILED] prefix — the status icon already conveys failure.
            title = re.sub(r"^\[FAILED\]\s*", "", title)
            titles[step_id] = title
        except (ValueError, IndexError):
            continue

    return titles

# Status icon helpers
_STATUS_ICONS: dict[StepStatus, str] = {
    StepStatus.PENDING:     "[ ]",
    StepStatus.IN_PROGRESS: "[~]",
    StepStatus.COMPLETE:    "[✓]",
    StepStatus.SKIPPED:     "[-]",
}

_STATUS_COLORS: dict[StepStatus, str] = {
    StepStatus.PENDING:     "#e2e8f0",
    StepStatus.IN_PROGRESS: "#5865f2",
    StepStatus.COMPLETE:    "#22c55e",
    StepStatus.SKIPPED:     "#64748b",
}

# StepRow
class StepRow(Widget, can_focus=True):
    """
    One row in the step list.

    Layout:
        [icon] NNN  Title text
        (accordion — only when expanded and step is COMPLETE/SKIPPED)
            Saved reasoning text (read-only, indented)
    """

    DEFAULT_CSS = """
    StepRow {
        width: 1fr;
        height: auto;
        padding: 0 1;
        background: #0f1117;
        border-bottom: round #2a2d3a;
    }
    StepRow:focus {
        background: #1a1d27;
    }
    StepRow.selected {
        background: #2a2d3a;
    }
    StepRow.in-progress {
        background: #0f1117;
        border-left: thick #5865f2;
    }
    StepRow .accordion {
        padding: 0 2 0 6;
        color: #64748b;
        background: #1a1d27;
        height: auto;
        display: none;
    }
    StepRow .accordion.visible {
        display: block;
    }
    """

    # True when the accordion is open.
    expanded: reactive[bool] = reactive(False)

    def __init__(self, step: StepState, title: str, index: int) -> None:
        super().__init__(id=f"step-row-{step.step_id}")
        self.step = step
        self.title = title
        self.index = index  # 0-based position in the list

    def compose(self) -> ComposeResult:
        icon = _STATUS_ICONS[self.step.status]
        color = _STATUS_COLORS[self.step.status]
        num = f"{self.step.step_id:>4}"
        title = self.title or f"{self.step.action_type} / {self.step.action_subtype}"
        yield Label(
            f"[{color}]{icon}[/{color}] {num}  {title}",
            id="row-label",
        )
        # Accordion — only rendered when step has saved reasoning.
        reasoning = self.step.reasoning or ""
        yield Static(
            reasoning or "(no reasoning saved)",
            classes="accordion",
            id="accordion-body",
        )

    def watch_expanded(self, value: bool) -> None:
        """Show or hide the accordion body."""
        try:
            body = self.query_one("#accordion-body")
            if value:
                body.add_class("visible")
            else:
                body.remove_class("visible")
        except NoMatches:
            pass

    def refresh_label(self) -> None:
        """Re-render the status icon + title after status change."""
        icon = _STATUS_ICONS[self.step.status]
        color = _STATUS_COLORS[self.step.status]
        num = f"{self.step.step_id:>4}"
        title = self.title or f"{self.step.action_type} / {self.step.action_subtype}"
        try:
            self.query_one("#row-label", Label).update(
                f"[{color}]{icon}[/{color}] {num}  {title}"
            )
        except NoMatches:
            pass

        # Keep CSS class in sync.
        if self.step.status == StepStatus.IN_PROGRESS:
            self.add_class("in-progress")
        else:
            self.remove_class("in-progress")

    def refresh_accordion(self) -> None:
        """Update the accordion body text (called after reasoning is saved)."""
        try:
            body = self.query_one("#accordion-body", Static)
            body.update(self.step.reasoning or "(no reasoning saved)")
        except NoMatches:
            pass

    # Mouse events
    def on_click(self, event) -> None:
        event.stop()
        self.post_message(StepList.StepSelected(self.step.step_id))

    def on_double_click(self, event) -> None:
        event.stop()
        self.post_message(StepList.StepEditRequested(self.step.step_id))


# StepList
class StepList(Widget, can_focus=True):
    """
    Virtual scrolling list of all steps.

    Keyboard:
        j / ↓         — move selection down
        k / ↑         — move selection up
        PgDn / PgUp   — jump 10 rows
        Enter / e     — request edit on selected step
        Space         — toggle accordion on COMPLETE or SKIPPED step

    Posts:
        StepList.StepSelected(step_id)
        StepList.StepEditRequested(step_id)
    """

    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("j,down",    "move_down",    "Down",    show=False),
        Binding("k,up",      "move_up",      "Up",      show=False),
        Binding("pagedown",  "page_down",    "PgDn",    show=False),
        Binding("pageup",    "page_up",      "PgUp",    show=False),
        Binding("enter,e",   "edit_current", "Edit",    show=False),
        Binding("space",     "toggle_accordion", "Expand", show=False),
    ]

    DEFAULT_CSS = """
    StepList {
        width: 1fr;
        height: 1fr;
        border: round #2a2d3a;
        background: #0f1117;
    }
    StepList > #steps-scroll {
        width: 1fr;
        height: 1fr;
        overflow-y: auto;
        border: round #2a2d3a;
        margin: 0 1 0 1;
        background: #0f1117;
    }
    """

    # Messages
    class StepSelected(Message):
        """Posted when the selection moves to a different step."""
        def __init__(self, step_id: int) -> None:
            super().__init__()
            self.step_id = step_id

    class StepEditRequested(Message):
        """Posted when the user presses Enter/e or double-clicks a row."""
        def __init__(self, step_id: int) -> None:
            super().__init__()
            self.step_id = step_id

    # Init
    def __init__(self, session_data: SessionState) -> None:
        super().__init__()
        self._session_data = session_data
        self._titles: dict[int, str] = {}
        self._rows: list[StepRow] = []
        # 0-based index of the currently selected row.
        self._cursor: int = session_data.cursor_step

    def compose(self) -> ComposeResult:
        # Load titles from converted_input.md once on compose.
        self._titles = load_converted_titles(self._session_data.memory_dir)

        self._rows = []
        with VerticalScroll(id="steps-scroll"):
            for i, step in enumerate(self._session_data.steps):
                title = self._titles.get(step.step_id, "")
                row = StepRow(step, title, i)
                self._rows.append(row)
                yield row

    def on_mount(self) -> None:
        self._apply_cursor(scroll=True)

    # Public API (called by AnnotationScreen)
    def select_step(self, step_id: int, scroll: bool = True) -> None:
        """Move selection to the row with the given step_id."""
        for i, row in enumerate(self._rows):
            if row.step.step_id == step_id:
                self._cursor = i
                self._apply_cursor(scroll=scroll)
                return

    def refresh_step(self, step_id: int) -> None:
        """Re-render a row after its status or reasoning changes."""
        for row in self._rows:
            if row.step.step_id == step_id:
                row.refresh_label()
                row.refresh_accordion()
                return

    def current_step_id(self) -> int | None:
        """Return the step_id of the currently selected row, or None."""
        if 0 <= self._cursor < len(self._rows):
            return self._rows[self._cursor].step.step_id
        return None

    # Cursor management
    def _apply_cursor(self, scroll: bool = False) -> None:
        """Update CSS classes and optionally scroll the selected row into view."""
        for i, row in enumerate(self._rows):
            if i == self._cursor:
                row.add_class("selected")
                row.focus()
                if scroll:
                    row.scroll_visible(animate=False)
            else:
                row.remove_class("selected")

    def _move(self, delta: int) -> None:
        new = max(0, min(len(self._rows) - 1, self._cursor + delta))
        if new != self._cursor:
            self._cursor = new
            self._apply_cursor(scroll=True)
            step_id = self._rows[self._cursor].step.step_id
            self.post_message(StepList.StepSelected(step_id))

    # Actions
    def action_move_down(self)  -> None: self._move(+1)
    def action_move_up(self)    -> None: self._move(-1)
    def action_page_down(self)  -> None: self._move(+10)
    def action_page_up(self)    -> None: self._move(-10)

    def action_edit_current(self) -> None:
        if 0 <= self._cursor < len(self._rows):
            step_id = self._rows[self._cursor].step.step_id
            self.post_message(StepList.StepEditRequested(step_id))

    def action_toggle_accordion(self) -> None:
        """Toggle accordion on the current row if it has saved content."""
        if not (0 <= self._cursor < len(self._rows)):
            return
        row = self._rows[self._cursor]
        if row.step.status in (StepStatus.COMPLETE, StepStatus.SKIPPED):
            row.expanded = not row.expanded