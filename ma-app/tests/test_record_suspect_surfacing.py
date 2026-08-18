"""ma-core's record-fidelity flag has to reach the annotator, not just the file.

A truncated command record is dangerous precisely because nothing about it looks
wrong: the step ran, the artifact on disk is correct, and every self-reporting
signal says success. The one moment a person looks closely at a step is while
annotating it, so a flag that lives only in `metadata.json` is a flag nobody
reads. These tests pin the path from the file to the banner.

The flag is advisory throughout. It must never alter the recorded command and
must never stop a step being annotated — a heuristic that blocks work would be
turned off, and then it protects nothing.
"""
from __future__ import annotations

import json
from pathlib import Path

from ma_app.tui.session_loader import SessionLoader
from ma_app.tui.widgets.image_review import ImageReview


SUSPECT = {
    "check": "dangling-escape",
    "note": "The recorded text ends in a lone backslash. Actuation is unaffected.",
}


def _session(tmp_path: Path, steps: list[dict]) -> Path:
    d = tmp_path / "a-session"
    (d / "commands").mkdir(parents=True)
    (d / "metadata.json").write_text(json.dumps({"steps": steps}), encoding="utf-8")
    return d


def _step(step_id: int, **kw) -> dict:
    base = {
        "step_id": step_id,
        "timestamp": "2026-07-22T08:09:41.000Z",
        "action_type": "keyboard",
        "action_subtype": "type",
        "image_path": None,
        "image_fetched": False,
        "marked": False,
    }
    base.update(kw)
    return base


def _load(d: Path):
    loader = SessionLoader(session_id="test-session")
    return {s.step_id: s for s in loader._build_steps(
        json.loads((d / "metadata.json").read_text(encoding="utf-8")), d
    )}


def test_the_flag_survives_the_load(tmp_path: Path) -> None:
    d = _session(tmp_path, [
        _step(2, raw_command="Typed: printf \\", record_suspect=SUSPECT),
    ])
    step = _load(d)[2]
    assert step.record_suspect == SUSPECT
    # And the record it describes is untouched.
    assert step.raw_command == "Typed: printf \\"


def test_an_unflagged_step_carries_none(tmp_path: Path) -> None:
    d = _session(tmp_path, [_step(1, raw_command="Typed: hello")])
    assert _load(d)[1].record_suspect is None


def test_a_malformed_flag_is_ignored_rather_than_crashing(tmp_path: Path) -> None:
    """metadata.json is edited by hand during repairs. A wrong shape in that
    field must not take the annotation session down — the step still has to be
    annotatable, which matters more than the advisory."""
    for bad in ["truncated", 42, [], True]:
        d = _session(tmp_path / f"x{bad!r}", [
            _step(1, raw_command="Typed: hello", record_suspect=bad),
        ])
        assert _load(d)[1].record_suspect is None


class _FakeStep:
    """Minimal stand-in — the banner only reads `record_suspect`."""

    def __init__(self, suspect: object) -> None:
        self.record_suspect = suspect


class _FakeBanner:
    def __init__(self) -> None:
        self.display = True
        self.text = "unset"

    def update(self, text: str) -> None:
        self.text = text


def _banner_for(suspect: object) -> _FakeBanner:
    review = ImageReview.__new__(ImageReview)
    review._step = _FakeStep(suspect)  # type: ignore[attr-defined]
    banner = _FakeBanner()
    review.query_one = lambda *a, **k: banner  # type: ignore[assignment]
    review._update_suspect_banner()
    return banner


def test_a_flagged_step_shows_the_banner() -> None:
    banner = _banner_for(SUSPECT)
    assert banner.display is True
    assert "dangling-escape" in banner.text
    # The wording must not let the annotator conclude the step failed.
    assert "keystrokes were delivered" in banner.text
    assert "frames" in banner.text


def test_an_unflagged_step_hides_the_banner() -> None:
    banner = _banner_for(None)
    assert banner.display is False
    assert banner.text == ""


def test_a_malformed_flag_hides_the_banner() -> None:
    for bad in ["truncated", 42, [], None]:
        assert _banner_for(bad).display is False
