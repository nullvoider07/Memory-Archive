"""Tests for Ctrl+N step advancement in the annotation screen.

The reported defect: an annotator who starts part-way through a session leaves
earlier steps pending, and Ctrl+N — which only ever scanned forward — could
never reach them again. Worse, on the last step the forward scan found nothing,
`all_done` was False because of that pending step, and the fallback branch did
nothing at all: no navigation, no completion prompt, no message. Ctrl+N looked
broken and the session could not be finished from the keyboard.

`_advance_to_next_pending` touches only the three panes it queries, so the
screen is driven here with stubs rather than a mounted app.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from ma_app.tui.screens.annotation import AnnotationScreen
from ma_app.tui.session_loader import SessionState, StepState, StepStatus
from ma_app.tui.widgets.image_review import ImageReview
from ma_app.tui.widgets.reasoning_editor import ReasoningEditor
from ma_app.tui.widgets.step_list import StepList


class FakeStepList:
    def __init__(self, current: int) -> None:
        self._current = current
        self.selected: list[int] = []
        self.refreshed: list[int] = []

    def current_step_id(self) -> int:
        return self._current

    def select_step(self, step_id: int, scroll: bool = False) -> None:
        self._current = step_id
        self.selected.append(step_id)

    def refresh_step(self, step_id: int) -> None:
        self.refreshed.append(step_id)


class FakeEditor:
    def __init__(self) -> None:
        self.edited: list[int] = []

    def enter_edit_mode(self, step) -> None:
        self.edited.append(step.step_id)


class FakeImagePane:
    def __init__(self) -> None:
        self.loaded: list[int] = []
        self.prefetched: list[int] = []

    def load_step(self, step) -> None:
        self.loaded.append(step.step_id)

    def prefetch_step(self, step) -> None:
        self.prefetched.append(step.step_id)


class ScreenHarness:
    """
    Calls the real `_advance_to_next_pending` with stubbed panes.

    The method is invoked unbound: AnnotationScreen is a Textual Screen and
    constructing one outside a running app trips its reactives.
    """

    def __init__(self, statuses: list[StepStatus], current_step_id: int) -> None:
        self._session_data = SessionState(
            session_id="test-session",
            memory_dir=Path("/nonexistent"),
            memory_name="test",
            mode="manual",
            total_steps=len(statuses),
            annotated_steps=0,
            skipped_steps=0,
            steps=[
                StepState(
                    step_id=i + 1,
                    timestamp="2026-08-04T00:00:00Z",
                    action_type="keyboard",
                    action_subtype="press",
                    image_path=None,
                    image_fetched=False,
                    marked=False,
                    status=status,
                )
                for i, status in enumerate(statuses)
            ],
        )
        self.step_list = FakeStepList(current_step_id)
        self.editor = FakeEditor()
        self.image_pane = FakeImagePane()
        self.flashes: list[str] = []
        self.completion_prompt_shown = False

    def query_one(self, widget_type):
        return {
            StepList: self.step_list,
            ReasoningEditor: self.editor,
            ImageReview: self.image_pane,
        }[widget_type]

    def update_stats(self, current_step_id=None, save_flash: str = "") -> None:
        if save_flash:
            self.flashes.append(save_flash)

    def call_after_refresh(self, callback) -> None:
        self.completion_prompt_shown = True

    def _show_completion_prompt(self) -> None:  # referenced by the real method
        self.completion_prompt_shown = True

    def advance(self) -> None:
        AnnotationScreen._advance_to_next_pending(self)  # type: ignore[arg-type]


C = StepStatus.COMPLETE
P = StepStatus.PENDING
S = StepStatus.SKIPPED


def test_forward_is_still_the_normal_case():
    """Ordinary advancement must be unchanged: next pending step, no message."""
    harness = ScreenHarness([C, C, P, P], current_step_id=2)
    harness.advance()
    assert harness.step_list.selected == [3]
    assert harness.editor.edited == [3]
    assert harness.image_pane.loaded == [3]
    assert harness.flashes == [], "a plain forward move must not announce itself"


def test_forward_prefetches_the_step_after_the_one_it_opened():
    harness = ScreenHarness([C, P, P], current_step_id=1)
    harness.advance()
    assert harness.image_pane.loaded == [2]
    assert harness.image_pane.prefetched == [3]


def test_it_wraps_back_to_a_pending_step_left_behind():
    """
    The reported case: annotation started at step 2, so step 1 stayed pending.
    From the last step, Ctrl+N must reach step 1 rather than stall.
    """
    harness = ScreenHarness([P, C, C, C], current_step_id=4)
    harness.advance()
    assert harness.step_list.selected == [1]
    assert harness.editor.edited == [1]
    assert harness.image_pane.loaded == [1]


def test_the_wrap_is_announced():
    """A jump backwards is otherwise indistinguishable from a misfire."""
    harness = ScreenHarness([P, C, C], current_step_id=3)
    harness.advance()
    assert harness.flashes == ["↩ Back to step 1"]


def test_the_wrap_takes_the_earliest_pending_step():
    harness = ScreenHarness([P, P, C, C], current_step_id=4)
    harness.advance()
    assert harness.step_list.selected == [1]


def test_forward_wins_over_wrapping():
    """A pending step ahead is always preferred to one behind."""
    harness = ScreenHarness([P, C, P, C], current_step_id=2)
    harness.advance()
    assert harness.step_list.selected == [3]
    assert harness.flashes == []


def test_a_fully_annotated_session_reaches_the_completion_prompt():
    harness = ScreenHarness([C, C, S, C], current_step_id=4)
    harness.advance()
    assert harness.completion_prompt_shown
    assert harness.step_list.selected == [], "nothing left to open"


def test_skipped_steps_are_not_revisited_by_the_wrap():
    """Skipped is a decision, not an omission — wrapping must not undo it."""
    harness = ScreenHarness([S, C, C], current_step_id=3)
    harness.advance()
    assert harness.step_list.selected == []
    assert harness.completion_prompt_shown


def test_the_last_pending_step_says_so_instead_of_doing_nothing():
    """
    The silent branch. The current step is the only one left pending, so there
    is nowhere to go — but Ctrl+N must still report that, not look dead.
    """
    harness = ScreenHarness([C, C, P], current_step_id=3)
    harness.advance()
    assert harness.step_list.selected == []
    assert harness.completion_prompt_shown is False
    assert harness.flashes == ["No other step is pending"]


def test_an_unknown_current_step_is_a_no_op():
    harness = ScreenHarness([C, P], current_step_id=99)
    harness.advance()
    assert harness.step_list.selected == []
    assert harness.flashes == []


@pytest.mark.parametrize("current", [1, 2, 3, 4])
def test_wrapping_never_selects_the_current_step(current):
    """
    The wrap scans range(0, current_idx), so the cursor step is excluded from
    both passes — re-opening it would be an infinite Ctrl+N loop.
    """
    harness = ScreenHarness([P, P, P, P], current_step_id=current)
    harness.advance()
    assert current not in harness.step_list.selected
