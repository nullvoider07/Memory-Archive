# /Memory-Archive/ma-app/ma_app/tui/screens/annotation.py

from __future__ import annotations

from typing import ClassVar, Optional

from textual.app import ComposeResult
from textual.binding import Binding, BindingType
from textual.css.query import NoMatches
from textual.screen import ModalScreen, Screen
from textual.widget import Widget
from textual.widgets import Button, Input, Label, Static

from ma_app.tui.session_loader import SessionState, StepState, StepStatus
from ma_app.tui.reasoning_writer import ReasoningWriter
from ma_app.tui.screens.help_overlay import HelpOverlay
from ma_app.tui.widgets.step_list import StepList
from ma_app.tui.widgets.reasoning_editor import ReasoningEditor
from ma_app.tui.widgets.image_review import ImageReview
from ma_app.tui.widgets.stats_pane import StatsPane

# Key hints bar
class KeyHintsBar(Widget):
    """Single-row keyboard reference pinned to the bottom of the screen."""

    DEFAULT_CSS = """
    KeyHintsBar {
        width: 1fr;
        height: 1;
        background: #1a1d27;
        color: #64748b;
        padding: 0 1;
        dock: bottom;
    }
    """

    def compose(self) -> ComposeResult:
        yield Label(
            "[#5865f2]Ctrl+N[/#5865f2] Next  "
            "[#5865f2]Ctrl+S[/#5865f2] Save  "
            "[#5865f2]Tab[/#5865f2] Switch pane  "
            "[#5865f2]?[/#5865f2] Help  "
            "[#5865f2]Ctrl+Q[/#5865f2] Quit  "
            "[#5865f2]Q/Esc[/#5865f2] Exit Fullscreen  "
            "[#5865f2]Ctrl+Shift+E[/#5865f2] Jump  "
            "[#5865f2]+/-[/#5865f2] Zoom  "
            "[#5865f2]f[/#5865f2] Fit  "
            "[#5865f2]u[/#5865f2] Revert"
        )


# Overlays
class JumpToStepOverlay(ModalScreen):
    """Prompts for a step number. Dismissed with int or None."""

    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("escape", "cancel", show=False),
    ]

    DEFAULT_CSS = """
    JumpToStepOverlay { align: center middle; }
    #jump-box {
        width: 44; height: auto;
        background: #1a1d27; border: round #5865f2; padding: 1 2;
    }
    #jump-label { width: 1fr; margin-bottom: 1; color: #e2e8f0; }
    #jump-input { width: 1fr; }
    #jump-hint  { width: 1fr; color: #64748b; margin-top: 1; }
    """

    def __init__(self, total_steps: int) -> None:
        super().__init__()
        self._total_steps = total_steps

    def compose(self) -> ComposeResult:
        with Widget(id="jump-box"):
            yield Label(f"Jump to step  (1 – {self._total_steps})", id="jump-label")
            yield Input(placeholder="Step number…", id="jump-input")
            yield Label("Enter to confirm  ·  Esc to cancel", id="jump-hint")

    def on_mount(self) -> None:
        try:
            self.query_one("#jump-input", Input).focus()
        except NoMatches:
            pass

    def on_input_submitted(self, event: Input.Submitted) -> None:
        try:
            step_id = int(event.value.strip())
        except ValueError:
            self._shake()
            return
        if not (1 <= step_id <= self._total_steps):
            self._shake()
            return
        self.dismiss(step_id)

    def action_cancel(self) -> None:
        self.dismiss(None)

    def _shake(self) -> None:
        try:
            inp = self.query_one("#jump-input", Input)
            inp.value = ""
            inp.focus()
        except NoMatches:
            pass


class QuitConfirmOverlay(ModalScreen):
    """Unsaved-changes confirmation. Dismissed with True (quit) or False."""

    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("escape", "cancel",  show=False),
        Binding("q",      "confirm", show=False),
        Binding("ctrl+q", "confirm", show=False),
    ]

    DEFAULT_CSS = """
    QuitConfirmOverlay { align: center middle; }
    #quit-box {
        width: 50; height: auto;
        background: #1a1d27; border: round #ef4444; padding: 1 2;
    }
    #quit-title {
        width: 1fr; content-align: center middle;
        text-style: bold; color: #ef4444; margin-bottom: 1;
    }
    #quit-body  { width: 1fr; color: #e2e8f0; margin-bottom: 1; }
    #quit-buttons {
        width: 1fr; height: auto;
        layout: horizontal; align: center middle;
    }
    #quit-buttons Button { margin: 0 1; }
    """

    def compose(self) -> ComposeResult:
        with Widget(id="quit-box"):
            yield Label("Unsaved changes", id="quit-title")
            yield Label(
                "The current step has unsaved reasoning.\n"
                "Quit anyway and lose these changes?",
                id="quit-body",
            )
            with Widget(id="quit-buttons"):
                yield Button("Quit  [q]",     id="btn-quit",   variant="error")
                yield Button("Cancel  [Esc]", id="btn-cancel", variant="primary")

    def on_mount(self) -> None:
        try:
            self.query_one("#btn-cancel", Button).focus()
        except NoMatches:
            pass

    def on_button_pressed(self, event: Button.Pressed) -> None:
        self.dismiss(event.button.id == "btn-quit")

    def action_confirm(self) -> None:
        self.dismiss(True)

    def action_cancel(self) -> None:
        self.dismiss(False)


class CrashRecoveryOverlay(ModalScreen):
    """
    Shown on TUI launch when the previous session was killed mid-annotation.
    Dismissed with True (resume at cursor step) or False (go to step 1).
    """

    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("escape", "start_fresh", show=False),
        Binding("r",      "resume",      show=False),
    ]

    DEFAULT_CSS = """
    CrashRecoveryOverlay { align: center middle; }
    #recovery-box {
        width: 56; height: auto;
        background: #1a1d27; border: round #f59e0b; padding: 1 2;
    }
    #recovery-title {
        width: 1fr; content-align: center middle;
        text-style: bold; color: #f59e0b; margin-bottom: 1;
    }
    #recovery-body  { width: 1fr; color: #e2e8f0; margin-bottom: 1; }
    #recovery-buttons {
        width: 1fr; height: auto;
        layout: horizontal; align: center middle;
    }
    #recovery-buttons Button {
        margin: 0 1;
        height: 3;
        min-width: 20;
        background: #0f1117;
        color: #e2e8f0;
        border: round #64748b;
    }
    #recovery-buttons Button:hover {
        background: #1a1d27;
        border: round #e2e8f0;
        color: #ffffff;
    }
    #recovery-buttons Button:focus {
        background: #1a1d27;
        border: round #5865f2;
        color: #ffffff;
    }
    """

    def __init__(self, step_id: int) -> None:
        super().__init__()
        self._step_id = step_id

    def compose(self) -> ComposeResult:
        with Widget(id="recovery-box"):
            yield Label("Previous session interrupted", id="recovery-title")
            yield Label(
                f"Step {self._step_id} was being edited when the TUI last closed.\n"
                "Resume from this step, or start review from step 1?",
                id="recovery-body",
            )
            with Widget(id="recovery-buttons"):
                yield Button(
                    f"Resume step {self._step_id}  [r]",
                    id="btn-resume",
                )
                yield Button(
                    "Start from step 1  [Esc]",
                    id="btn-fresh",
                )

    def on_mount(self) -> None:
        try:
            self.query_one("#btn-resume", Button).focus()
        except NoMatches:
            pass

    def on_button_pressed(self, event: Button.Pressed) -> None:
        self.dismiss(event.button.id == "btn-resume")

    def action_resume(self) -> None:
        self.dismiss(True)

    def action_start_fresh(self) -> None:
        self.dismiss(False)


class AnnotationCompleteOverlay(ModalScreen):
    """
    Shown when the last step is annotated or skipped.
    Dismissed with 'compile', 'later', or 'quit'.
    """

    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("c", "compile_now",   show=False),
        Binding("l", "compile_later", show=False),
        Binding("q", "quit_now",      show=False),
        Binding("escape", "quit_now", show=False),
    ]

    DEFAULT_CSS = """
    AnnotationCompleteOverlay {
        align: center middle;
        width: 100%;
        height: 100%;
        background: rgba(0, 0, 0, 0);
    }
    #complete-box {
        width: 72; height: auto;
        background: #1a1d27; border: round #22c55e; padding: 1 2;
    }
    #complete-title {
        width: 1fr; content-align: center middle;
        text-style: bold; color: #22c55e; margin-bottom: 1;
    }
    #complete-body  { width: 1fr; color: #e2e8f0; margin-bottom: 1; }
    #complete-buttons {
        width: 1fr; height: auto;
        layout: horizontal; align: center middle;
    }
    #complete-buttons Button {
        margin: 0 1;
        height: 3;
        min-width: 18;
        background: #0f1117;
        color: #e2e8f0;
        border: round #64748b;
    }
    #complete-buttons Button:hover {
        background: #1a1d27;
        border: round #e2e8f0;
        color: #ffffff;
    }
    #btn-compile:focus { background: #1a1d27; border: round #22c55e; color: #ffffff; }
    #btn-later:focus   { background: #1a1d27; border: round #5865f2; color: #ffffff; }
    #btn-quit:focus    { background: #1a1d27; border: round #ef4444; color: #ffffff; }
    """

    def __init__(self, annotated: int, skipped: int) -> None:
        super().__init__()
        self._annotated = annotated
        self._skipped   = skipped

    def compose(self) -> ComposeResult:
        with Widget(id="complete-box"):
            yield Label("All steps annotated", id="complete-title")
            yield Label(
                f"{self._annotated} annotated, {self._skipped} skipped.\n"
                "Ready to compile memory.md.",
                id="complete-body",
            )
            with Widget(id="complete-buttons"):
                yield Button("Compile now  [c]",   id="btn-compile")
                yield Button("Compile later  [l]", id="btn-later")
                yield Button("Quit  [q]",          id="btn-quit")

    def on_mount(self) -> None:
        try:
            self.query_one("#btn-compile", Button).focus()
        except NoMatches:
            pass

    def on_key(self, event) -> None:
        order = ["btn-compile", "btn-later", "btn-quit"]
        focused = self.focused
        if focused is None or not hasattr(focused, "id") or focused.id not in order:
            return
        idx = order.index(focused.id)
        if event.key == "right":
            event.stop()
            self.query_one(f"#{order[(idx + 1) % len(order)]}", Button).focus()
        elif event.key == "left":
            event.stop()
            self.query_one(f"#{order[(idx - 1) % len(order)]}", Button).focus()

    def on_button_pressed(self, event: Button.Pressed) -> None:
        mapping = {
            "btn-compile": "compile",
            "btn-later":   "later",
            "btn-quit":    "quit",
        }
        self.dismiss(mapping.get(event.button.id or "", "quit"))

    def action_compile_now(self)   -> None: self.dismiss("compile")
    def action_compile_later(self) -> None: self.dismiss("later")
    def action_quit_now(self)      -> None: self.dismiss("quit")

# Main annotation screen
class AnnotationScreen(Screen):
    """
    Fixed two-column layout — no draggable dividers.

        ┌── #left-col (70%) ──────────────────┬── #right-col (30%) ──┐
        │  ImageReview          (80% h)        │  StepList  (80% h)   │
        │                                      │                      │
        │  ReasoningEditor      (20% h)        │  StatsPane (20% h)   │
        └──────────────────────────────────────┴──────────────────────┘
        │  KeyHintsBar  (full width, docked bottom, 1 row)            │
        └─────────────────────────────────────────────────────────────┘

    Panel borders carry a border_title that renders as a tab label on the
    top border line (matching the wireframe).  Sizes are fixed in CSS.
    """

    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("ctrl+q",        "quit",         "Quit",         show=False),
        Binding("tab",           "cycle_focus",  "Switch pane",  show=False),
        Binding("question_mark", "show_help",    "Help",         show=False),
        Binding("ctrl+shift+e",  "jump_to_step", "Jump to step", show=False),
        Binding("u",             "revert_step",  "Revert",       show=False, priority=False),
    ]

    DEFAULT_CSS = """
    AnnotationScreen {
        layout: vertical;
        background: #0f1117;
    }

    #main-row {
        layout: horizontal;
        height: 1fr;
    }

    #left-col {
        width: 70%;
        height: 1fr;
        layout: vertical;
    }

    #right-col {
        width: 30%;
        height: 1fr;
        layout: vertical;
    }

    /* Panel borders — title appears as a tab on the top border line */
    ImageReview {
        border: round #2a2d3a;
        border-title-color: #e2e8f0;
        border-title-background: #0f1117;
        border-title-align: left;
    }
    StepList {
        border: round #2a2d3a;
        border-title-color: #e2e8f0;
        border-title-background: #0f1117;
        border-title-align: left;
    }
    ReasoningEditor {
        border: round #5865f2;
        border-title-color: #e2e8f0;
        border-title-background: #0f1117;
        border-title-align: left;
    }
    StatsPane {
        border: round #2a2d3a;
        border-title-color: #e2e8f0;
        border-title-background: #0f1117;
        border-title-align: left;
    }
    """

    def __init__(self, session_data: SessionState) -> None:
        super().__init__()
        self._session_data = session_data
        self._writer = ReasoningWriter(
            session_data.memory_dir,
            session_data.session_id,
            remote_fetcher=session_data.remote_fetcher,
        )

    # Compose
    def compose(self) -> ComposeResult:
        with Widget(id="main-row"):
            with Widget(id="left-col"):
                yield ImageReview(
                    self._session_data.memory_dir,
                    remote_fetcher=self._session_data.remote_fetcher,
                )
                yield ReasoningEditor()
            with Widget(id="right-col"):
                yield StepList(self._session_data)
                yield StatsPane()
        yield KeyHintsBar()

    def on_mount(self) -> None:
        # Inline styles override all DEFAULT_CSS — safest way to enforce splits.
        self.query_one(ImageReview).styles.height     = "4fr"
        self.query_one(ReasoningEditor).styles.height = "1fr"
        self.query_one(StepList).styles.height        = "4fr"
        self.query_one(StatsPane).styles.height       = "1fr"

        self.query_one(ImageReview).border_title     = "Image"
        self.query_one(StepList).border_title        = "Steps to Annotate"
        self.query_one(ReasoningEditor).border_title = "Reasoning"
        self.query_one(StatsPane).border_title       = "Stats"

        self._load_cursor_step()

        if self._session_data.was_interrupted:
            self._show_crash_recovery_prompt()

    # Stats
    def update_stats(
        self,
        current_step_id: Optional[int] = None,
        save_flash: str = "",
    ) -> None:
        try:
            self.query_one(StatsPane).update(
                self._session_data,
                current_step_id=current_step_id,
                save_flash=save_flash,
            )
        except NoMatches:
            pass

    # Crash recovery
    def _show_crash_recovery_prompt(self) -> None:
        steps = self._session_data.steps
        if not steps:
            return
        cursor_idx = max(0, min(self._session_data.cursor_step, len(steps) - 1))
        step_id    = steps[cursor_idx].step_id

        def _push() -> None:
            self.app.push_screen(
                CrashRecoveryOverlay(step_id),
                self._on_crash_recovery,
            )

        self.call_after_refresh(_push)

    def _on_crash_recovery(self, resume: bool | None = None) -> None:
        if resume:
            return
        steps = self._session_data.steps
        if not steps:
            return
        first = steps[0]
        try:
            self.query_one(StepList).select_step(first.step_id, scroll=True)
            self.query_one(ReasoningEditor).load_step(first)
            self.query_one(ImageReview).load_step(first)
        except NoMatches:
            pass
        self.update_stats(current_step_id=first.step_id)

    # Session helpers
    def _get_step(self, step_id: int) -> Optional[StepState]:
        for step in self._session_data.steps:
            if step.step_id == step_id:
                return step
        return None

    def _update_counters(self) -> None:
        self._session_data.annotated_steps = sum(
            1 for s in self._session_data.steps if s.status == StepStatus.COMPLETE
        )
        self._session_data.skipped_steps = sum(
            1 for s in self._session_data.steps if s.status == StepStatus.SKIPPED
        )

    def _prefetch_next_after(self, current_step_id: int) -> None:
        """
        Pre-fetch images for the next pending or in-progress step after
        current_step_id. No-op in local mode (RemoteFetcher is None).
        """
        steps = self._session_data.steps
        try:
            image_pane = self.query_one(ImageReview)
        except NoMatches:
            return
        current_idx = next(
            (i for i, s in enumerate(steps) if s.step_id == current_step_id), None
        )
        if current_idx is None:
            return
        for j in range(current_idx + 1, len(steps)):
            candidate = steps[j]
            if candidate.status in (StepStatus.PENDING, StepStatus.IN_PROGRESS):
                image_pane.prefetch_step(candidate)
                break

    def _load_cursor_step(self) -> None:
        steps = self._session_data.steps
        if not steps:
            self.update_stats()
            return
        cursor_idx = max(0, min(self._session_data.cursor_step, len(steps) - 1))
        step = steps[cursor_idx]
        try:
            self.query_one(ReasoningEditor).load_step(step)
            self.query_one(ImageReview).load_step(step)
        except NoMatches:
            pass
        self.update_stats(current_step_id=step.step_id)
        self._prefetch_next_after(step.step_id)

    def _advance_to_next_pending(self) -> None:
        steps = self._session_data.steps
        try:
            step_list  = self.query_one(StepList)
            editor     = self.query_one(ReasoningEditor)
            image_pane = self.query_one(ImageReview)
        except NoMatches:
            return

        current_id  = step_list.current_step_id()
        current_idx = next(
            (i for i, s in enumerate(steps) if s.step_id == current_id), None
        )
        if current_idx is None:
            return

        for i in range(current_idx + 1, len(steps)):
            candidate = steps[i]
            if candidate.status in (StepStatus.PENDING, StepStatus.IN_PROGRESS):
                candidate.status = StepStatus.IN_PROGRESS
                step_list.select_step(candidate.step_id, scroll=True)
                step_list.refresh_step(candidate.step_id)
                editor.enter_edit_mode(candidate)
                image_pane.load_step(candidate)
                for j in range(i + 1, len(steps)):
                    next_step = steps[j]
                    if next_step.status in (StepStatus.PENDING, StepStatus.IN_PROGRESS):
                        image_pane.prefetch_step(next_step)
                        break
                self.update_stats(current_step_id=candidate.step_id)
                return

        all_done = all(
            s.status in (StepStatus.COMPLETE, StepStatus.SKIPPED)
            for s in steps
        )
        if all_done:
            self.call_after_refresh(self._show_completion_prompt)
        else:
            self.update_stats(current_step_id=current_id)

    # StepList message handlers
    def on_step_list_step_selected(self, msg: StepList.StepSelected) -> None:
        step = self._get_step(msg.step_id)
        if step is None:
            return
        try:
            self.query_one(ReasoningEditor).load_step(step)
            self.query_one(ImageReview).load_step(step)
        except NoMatches:
            pass
        self.update_stats(current_step_id=msg.step_id)
        self._prefetch_next_after(msg.step_id)

    def on_step_list_step_edit_requested(self, msg: StepList.StepEditRequested) -> None:
        step = self._get_step(msg.step_id)
        if step is None:
            return
        if step.status == StepStatus.PENDING:
            step.status = StepStatus.IN_PROGRESS
            try:
                self.query_one(StepList).refresh_step(msg.step_id)
            except NoMatches:
                pass
        try:
            self.query_one(ReasoningEditor).enter_edit_mode(step)
            self.query_one(ImageReview).load_step(step)
        except NoMatches:
            pass
        self.update_stats(current_step_id=msg.step_id)

    # ReasoningEditor message handlers
    def on_reasoning_editor_step_saved(self, msg: ReasoningEditor.StepSaved) -> None:
        step = self._get_step(msg.step_id)
        if step is None:
            return
        step.reasoning = msg.reasoning
        self._writer.write_entry(step)
        try:
            self.query_one(ReasoningEditor).mark_saved(msg.step_id, msg.reasoning)
            self.query_one(StepList).refresh_step(msg.step_id)
        except NoMatches:
            pass
        self.update_stats(current_step_id=msg.step_id, save_flash="✓ Saved")
        self._flash_reasoning_title("✓ Saved")

    def on_reasoning_editor_step_completed(
        self, msg: ReasoningEditor.StepCompleted
    ) -> None:
        step = self._get_step(msg.step_id)
        if step is None:
            return
        step.reasoning = msg.reasoning
        step.status    = StepStatus.COMPLETE
        self._update_counters()
        self._writer.write_entry(step)
        self._writer.sync_counters(
            self._session_data.annotated_steps,
            self._session_data.skipped_steps,
        )
        try:
            self.query_one(ReasoningEditor).mark_saved(msg.step_id, msg.reasoning)
            self.query_one(StepList).refresh_step(msg.step_id)
        except NoMatches:
            pass
        self.update_stats(current_step_id=msg.step_id, save_flash="✓ Saved")
        self._advance_to_next_pending()

    def on_reasoning_editor_step_skipped(
        self, msg: ReasoningEditor.StepSkipped
    ) -> None:
        step = self._get_step(msg.step_id)
        if step is None:
            return
        step.reasoning = ""
        step.status    = StepStatus.SKIPPED
        self._update_counters()
        self._writer.write_entry(step)
        self._writer.sync_counters(
            self._session_data.annotated_steps,
            self._session_data.skipped_steps,
        )
        try:
            self.query_one(StepList).refresh_step(msg.step_id)
        except NoMatches:
            pass
        self._advance_to_next_pending()

    def _show_completion_prompt(self) -> None:
        self.app.push_screen(
            AnnotationCompleteOverlay(
                self._session_data.annotated_steps,
                self._session_data.skipped_steps,
            ),
            self._on_annotation_complete,
        )

    def _on_annotation_complete(self, choice: str | None = None) -> None:
        self._writer.complete_annotation()
        self.app.mark_annotation_complete()
        if choice == "compile":
            self.app.exit(result="compile")
        elif choice == "later":
            self.app.exit()
        else:
            self.app.exit()

    # Actions
    def action_quit(self) -> None:
        has_unsaved = False
        try:
            has_unsaved = self.query_one(ReasoningEditor).has_unsaved_draft()
        except NoMatches:
            pass
        if has_unsaved:
            self.app.push_screen(QuitConfirmOverlay(), self._on_quit_confirmed)
        else:
            self._quit_cleanly()

    def _on_quit_confirmed(self, confirmed: bool | None = None) -> None:
        if confirmed:
            self._quit_cleanly()

    def _quit_cleanly(self) -> None:
        self._writer.close_session()
        self.app.mark_session_closed()
        self.app.exit()

    def action_cycle_focus(self) -> None:
        focusable = [ImageReview, StepList, ReasoningEditor]
        focused = self.focused
        for i, cls in enumerate(focusable):
            try:
                widget = self.query_one(cls)
                if widget is focused or widget.has_focus or any(
                    c is focused for c in widget.walk_children()
                ):
                    next_cls = focusable[(i + 1) % len(focusable)]
                    nxt = self.query_one(next_cls)
                    if next_cls is ReasoningEditor:
                        try:
                            nxt.query_one("#editor-area").focus()
                        except NoMatches:
                            nxt.focus()
                    else:
                        nxt.focus()
                    return
            except NoMatches:
                continue
        try:
            self.query_one(ImageReview).focus()
        except NoMatches:
            pass

    def action_revert_step(self) -> None:
        try:
            self.query_one(ReasoningEditor).revert_current_step()
        except NoMatches:
            pass

    def action_show_help(self) -> None:
        self.app.push_screen(HelpOverlay())

    def action_jump_to_step(self) -> None:
        self.app.push_screen(
            JumpToStepOverlay(self._session_data.total_steps),
            self._on_jump_result,
        )

    def _on_jump_result(self, step_id: int | None = None) -> None:
        if step_id is None:
            return
        step = self._get_step(step_id)
        if step is None:
            return
        try:
            self.query_one(StepList).select_step(step_id, scroll=True)
            self.query_one(ReasoningEditor).enter_edit_mode(step)
            self.query_one(ImageReview).load_step(step)
        except NoMatches:
            pass
        self.update_stats(current_step_id=step_id)
        self._prefetch_next_after(step_id)

    def _flash_reasoning_title(self, text: str) -> None:
        """Flash the ReasoningEditor border_title briefly so the user sees the save."""
        try:
            editor = self.query_one(ReasoningEditor)
            editor.border_title = f"Reasoning  [#22c55e]{text}[/#22c55e]"
            self.set_timer(0.8, lambda: self._reset_reasoning_title())
        except NoMatches:
            pass

    def _reset_reasoning_title(self) -> None:
        try:
            self.query_one(ReasoningEditor).border_title = "Reasoning"
        except NoMatches:
            pass