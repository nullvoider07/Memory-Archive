# /Memory-Archive/ma-app/ma_app/tui/widgets/stats_pane.py

from __future__ import annotations

from typing import Optional

from rich.text import Text
from textual.app import ComposeResult
from textual.css.query import NoMatches
from textual.timer import Timer
from textual.widget import Widget
from textual.widgets import Label, Static

from ma_app.tui.session_loader import SessionState, StepState


class PillProgressBar(Widget):
    """
    A pill-shaped progress bar rendered entirely via Rich markup.

    Draws three terminal rows:
      top:    ╭───────────────────╮
      middle: │                   │  (green fill + dark empty)
      bottom: ╰───────────────────╯
    """

    DEFAULT_CSS = """
    PillProgressBar {
        width: 1fr;
        height: 3;
        background: transparent;
        border: none;
        padding: 0;
    }
    """

    BORDER    = "#3a3d4a"
    FILL_CLR  = "#22c55e"
    EMPTY_CLR = "#1a1d27"

    def __init__(self) -> None:
        super().__init__()
        self._pct: int = 0

    def set_pct(self, pct: int) -> None:
        self._pct = max(0, min(100, pct))
        self.refresh()

    def render(self) -> Text:
        w      = max(4, self.size.width)
        inner  = w - 2
        filled = int(inner * self._pct / 100)
        empty  = inner - filled

        top    = f"[{self.BORDER}]╭{'─' * inner}╮[/{self.BORDER}]"
        middle = (
            f"[{self.BORDER}]│[/{self.BORDER}]"
            f"[on {self.FILL_CLR}]{' ' * filled}[/on {self.FILL_CLR}]"
            f"[on {self.EMPTY_CLR}]{' ' * empty}[/on {self.EMPTY_CLR}]"
            f"[{self.BORDER}]│[/{self.BORDER}]"
        )
        bottom = f"[{self.BORDER}]╰{'─' * inner}╯[/{self.BORDER}]"

        return Text.from_markup(f"{top}\n{middle}\n{bottom}")


class StatsPane(Widget):
    """
    Bottom-right statistics panel.

    Layout (top to bottom):
      - Bordered info box: memory name + session id on row 1,
        annotated + skipped on row 2
      - Pill progress bar + percentage label
      - Current step metadata
      - Save flash label

    Public API:
        update(session, current_step_id, save_flash)  — called by AnnotationScreen
    """

    DEFAULT_CSS = """
    StatsPane {
        width: 1fr;
        height: 1fr;
        layout: vertical;
        border: round #2a2d3a;
        background: #0f1117;
        padding: 0 1;
    }

    StatsPane > #info-box {
        width: 1fr;
        height: auto;
        border: round #2a2d3a;
        padding: 0 1;
        margin: 0 0 0 0;
        color: #e2e8f0;
    }

    StatsPane > PillProgressBar {
        width: 1fr;
        height: 3;
        margin: 0;
    }

    StatsPane > #pct-label {
        width: 1fr;
        height: 1;
        color: #e2e8f0;
    }

    StatsPane > #step-section {
        width: 1fr;
        height: auto;
        padding: 1 0 0 0;
        color: #e2e8f0;
    }

    StatsPane > #flash-label {
        width: 1fr;
        height: 1;
        color: #22c55e;
        display: none;
    }

    StatsPane > #flash-label.visible {
        display: block;
    }
    """

    def __init__(self) -> None:
        super().__init__()
        self._flash_timer: Timer | None = None

    def compose(self) -> ComposeResult:
        yield Static("", id="info-box", markup=True)
        yield PillProgressBar()
        yield Label("", id="pct-label")
        yield Static("", id="step-section", markup=True)
        yield Label("", id="flash-label")

    def update(
        self,
        session: SessionState,
        current_step_id: Optional[int] = None,
        save_flash: str = "",
    ) -> None:
        total     = session.total_steps
        annotated = session.annotated_steps
        skipped   = session.skipped_steps
        done      = annotated + skipped
        pct       = int(done * 100 / total) if total > 0 else 0

        mem = session.memory_name
        if len(mem) > 18:
            mem = mem[:15] + "…"

        sid = session.session_id
        if len(sid) > 18:
            sid = sid[:7] + "…" + sid[-7:]

        mem_col = mem.ljust(18)
        sid_col = sid.ljust(18)

        info_text = (
            f"[#64748b]Name[/#64748b]      [#e2e8f0]{mem_col}[/#e2e8f0]"
            f"[#64748b]Annotated[/#64748b]  [#22c55e]{annotated}[/#22c55e]\n"
            f"[#64748b]Session[/#64748b]   [#64748b]{sid_col}[/#64748b]"
            f"[#64748b]Skipped[/#64748b]    [#f59e0b]{skipped}[/#f59e0b]"
        )

        try:
            self.query_one("#info-box", Static).update(info_text)
        except NoMatches:
            pass

        try:
            self.query_one(PillProgressBar).set_pct(pct)
        except NoMatches:
            pass

        try:
            self.query_one("#pct-label", Label).update(f"[#e2e8f0]{pct}%[/#e2e8f0]")
        except NoMatches:
            pass

        current_step: Optional[StepState] = None
        if current_step_id is not None:
            for s in session.steps:
                if s.step_id == current_step_id:
                    current_step = s
                    break

        step_text = ""
        if current_step is not None:
            step_text = (
                f"[#64748b]Step[/#64748b] [#e2e8f0]{current_step.step_id} / {total}[/#e2e8f0]  "
                f"[#64748b]Action[/#64748b] [#e2e8f0]{current_step.action_type}"
                f"/{current_step.action_subtype}[/#e2e8f0]"
            )

        try:
            self.query_one("#step-section", Static).update(step_text)
        except NoMatches:
            pass

        if save_flash:
            self._show_flash(save_flash)

    def _show_flash(self, text: str) -> None:
        try:
            label = self.query_one("#flash-label", Label)
            label.update(f"[#22c55e]{text}[/#22c55e]")
            label.add_class("visible")
        except NoMatches:
            return

        if self._flash_timer is not None:
            self._flash_timer.stop()
        self._flash_timer = self.set_timer(0.8, self._hide_flash)

    def _hide_flash(self) -> None:
        self._flash_timer = None
        try:
            self.query_one("#flash-label").remove_class("visible")
        except NoMatches:
            pass