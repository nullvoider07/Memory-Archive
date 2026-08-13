"""The visible step list must render the loader's command, not a derived file.

`SessionLoader` was made authoritative over `commands/converted_input.md` in
0.4.0, but the annotation screen's step list carried its own parser
(`load_converted_titles`) and read that file a second time. So the pane the
annotator actually reads went on showing the superseded text — `Hold at
(1393, 810)` for S007 step 8, months after `metadata.json` carried
`Drag from (344, 221) to (1393, 810)` — and truncated any command containing a
pipe, because it split every `|` and kept column 2.

Nothing reached `reasoning.jsonl` wrongly: the save path uses `StepState`. But
the annotator reasons from what is displayed, so a wrong label produces a
correct-looking annotation of an action that never happened. These tests pin the
row title to `StepState` and pin the second reader as removed.
"""
from __future__ import annotations

import inspect

from ma_app.tui.widgets import step_list as step_list_mod
from ma_app.tui.widgets.step_list import StepRow
from ma_app.tui.session_loader import StepState


def _step(**kw) -> StepState:
    base = dict(
        step_id=8,
        timestamp="2026-07-15T11:45:35.714Z",
        action_type="mouse",
        action_subtype="drag",
        image_path=None,
        image_fetched=False,
        marked=True,
    )
    base.update(kw)
    return StepState(**base)  # type: ignore[arg-type]


def test_title_is_the_loader_value_not_the_derived_file():
    # The exact S007 step-8 disagreement: metadata carries the corrected drag.
    step = _step(converted_command="Drag from (344, 221) to (1393, 810)")
    assert StepRow(step, 0)._title() == "Drag from (344, 221) to (1393, 810)"


def test_a_piped_command_is_not_truncated_in_the_list():
    piped = (
        "Get-ChildItem $env:USERPROFILE\\corpus-seed | "
        "Sort-Object Length -Descending | Select-Object -First 1 | Remove-Item"
    )
    row = StepRow(_step(action_subtype="type", converted_command=piped), 0)
    assert row._title() == piped
    assert row._title().count("|") == 3


def test_falls_back_to_the_action_pair_when_no_command_is_known():
    row = StepRow(_step(action_type="keyboard", action_subtype="press"), 0)
    assert row._title() == "keyboard / press"


def test_the_row_reads_the_step_at_render_time():
    # A row must not capture the title at construction: if the loader's value is
    # corrected, the next render has to show the correction.
    step = _step(converted_command="Hold at (1393, 810)")
    row = StepRow(step, 0)
    step.converted_command = "Drag from (344, 221) to (1393, 810)"
    assert row._title() == "Drag from (344, 221) to (1393, 810)"


def test_the_second_parser_is_gone():
    assert not hasattr(step_list_mod, "load_converted_titles"), (
        "the step list must not parse converted_input.md; the loader owns it"
    )


def test_the_widget_module_never_reads_the_derived_files():
    src = inspect.getsource(step_list_mod)
    body = "\n".join(
        line for line in src.splitlines() if not line.lstrip().startswith("#")
    )
    for derived in ("converted_input.md", "raw_input.md", "actuation_commands.json"):
        assert derived not in body, (
            f"{derived} is a derived view — the step list must render StepState"
        )
