"""metadata.json is authoritative for a step's commands, not the derived files.

Two ways the derived files under `commands/` disagree with `metadata.json`, both
found in the recorded corpus:

1. Six drag steps had their origin recovered by hand from the frames into
   `metadata.json`. The derived files were never regenerated, so
   `converted_input.md` still records the gesture the agent first misclassified —
   `Hold at (1393, 810)` for what was a drag from (344, 221).

2. `converted_input.md` is a markdown table delimited by `|`, and a piped shell
   command contains one. Written unescaped, the row gains a column and a reader
   splitting on `|` truncates the command at the first pipe. Three terminal
   sessions were recorded that way.

The loader used to overwrite the metadata value with the file value in both cases,
and the TUI writes whatever it holds into `reasoning.jsonl`, which is compiled into
training data. So a stale or truncated string became a plausible record of an
action that never happened.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from ma_app.tui.session_loader import SessionLoader


def _session(tmp_path: Path, steps: list[dict], converted_rows: list[str]) -> Path:
    d = tmp_path / "a-session"
    (d / "commands").mkdir(parents=True)
    (d / "metadata.json").write_text(json.dumps({"steps": steps}), encoding="utf-8")
    body = [
        "# Converted Input",
        "",
        "| Step | Timestamp | Action |",
        "|------|-----------|--------|",
        *converted_rows,
    ]
    (d / "commands" / "converted_input.md").write_text(
        "\n".join(body) + "\n", encoding="utf-8"
    )
    return d


def _step(step_id: int, **kw) -> dict:
    base = {
        "step_id": step_id,
        "timestamp": "2026-07-15T11:45:35.714Z",
        "action_type": "mouse",
        "action_subtype": "drag",
        "image_path": None,
        "image_fetched": False,
        "marked": True,
    }
    base.update(kw)
    return base


def _load(d: Path):
    loader = SessionLoader(session_id="test-session")
    return {s.step_id: s for s in loader._build_steps(
        json.loads((d / "metadata.json").read_text(encoding="utf-8")), d
    )}


def test_metadata_beats_a_stale_derived_file(tmp_path: Path) -> None:
    """The corrected drag in metadata must not be overwritten by the stale table."""
    d = _session(
        tmp_path,
        [_step(8, converted_command="Drag from (344, 221) to (1393, 810)")],
        ["|    8 | 2026-07-15T11:45:35.714Z | Hold at (1393, 810) |"],
    )
    assert _load(d)[8].converted_command == "Drag from (344, 221) to (1393, 810)"


def test_the_derived_file_still_fills_a_gap(tmp_path: Path) -> None:
    """Where metadata carries nothing, the table is still the source."""
    d = _session(
        tmp_path,
        [_step(3, action_type="keyboard", action_subtype="type")],
        ["|    3 | 2026-07-15T11:43:07.825Z | Type: corpus-seed |"],
    )
    assert _load(d)[3].converted_command == "Type: corpus-seed"


def test_an_escaped_pipe_survives_the_round_trip(tmp_path: Path) -> None:
    """ma-core escapes '|'; the reader must put it back, not split on it."""
    cmd = r"Type: Get-ChildItem -Filter *invoice* | Copy-Item -Destination found\\"
    escaped = cmd.replace("|", r"\|")
    d = _session(
        tmp_path,
        [_step(2, action_type="keyboard", action_subtype="type")],
        [f"|    2 | 2026-07-30T06:26:09.953Z | {escaped} |"],
    )
    assert _load(d)[2].converted_command == cmd


def test_an_unescaped_pipe_from_an_old_capture_is_not_truncated(tmp_path: Path) -> None:
    """Rows written before the escape fix must still read back whole.

    This is the shape actually on disk in three corpus sessions. Truncating here
    yields `Get-ChildItem -Filter *invoice*` — a command that runs, succeeds, and
    does not copy anything.
    """
    d = _session(
        tmp_path,
        [_step(2, action_type="keyboard", action_subtype="type")],
        [
            "|    2 | 2026-07-30T06:26:09.953Z | "
            "Type: Get-ChildItem -Filter *invoice* | Copy-Item -Destination found |"
        ],
    )
    got = _load(d)[2].converted_command
    assert got == "Type: Get-ChildItem -Filter *invoice* | Copy-Item -Destination found"
    assert "Copy-Item" in got, "the half after the pipe is the half that does the work"


@pytest.mark.parametrize(
    "row, expected",
    [
        ("|  1 | ts | plain |", ["1", "ts", "plain"]),
        (r"|  1 | ts | a \| b |", ["1", "ts", "a | b"]),
        (r"|  1 | ts | trailing \| |", ["1", "ts", "trailing |"]),
    ],
)
def test_split_md_row_handles_escapes(row: str, expected: list[str]) -> None:
    assert SessionLoader._split_md_row(row) == expected
