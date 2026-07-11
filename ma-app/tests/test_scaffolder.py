"""Tests for ma_app.compiler.scaffolder.run_compile.

Focus: run_compile must PRESERVE an existing memory.md rather than regenerate it,
so that resuming a compile-stage session (left at pending_compilation by a Ctrl+Q
quit) does not wipe the notes/edge-cases already written into the draft.
"""

from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

from rich.console import Console

from ma_app.compiler import scaffolder
from ma_app.compiler.scaffolder import run_compile


def _seed_session(memory_dir: Path) -> None:
    """Write a minimal valid metadata.json + reasoning.jsonl into memory_dir."""
    memory_dir.mkdir(parents=True, exist_ok=True)
    (memory_dir / "metadata.json").write_text(
        json.dumps(
            {
                "memory_name": "finder-nested-folders",
                "memory_description": "test",
                "session_id": "test-sid",
                "mode": "manual",
                "status": "complete",
                "os": {"type": "MACOS", "version": "macOS 15", "architecture": "x86_64"},
                "total_steps": 2,
                "steps": [
                    {"step_id": 1, "action_type": "mouse", "action_subtype": "left"},
                    {"step_id": 2, "action_type": "keyboard", "action_subtype": "type"},
                ],
            }
        ),
        encoding="utf-8",
    )
    reasoning_dir = memory_dir / "reasoning"
    reasoning_dir.mkdir(parents=True, exist_ok=True)
    (reasoning_dir / "reasoning.jsonl").write_text(
        "\n".join(
            json.dumps(r)
            for r in (
                {"step_id": 1, "reasoning": "first", "action_type": "mouse",
                 "action_subtype": "left", "skipped": False},
                {"step_id": 2, "reasoning": "second", "action_type": "keyboard",
                 "action_subtype": "type", "skipped": False},
            )
        ),
        encoding="utf-8",
    )


def _patch_ipc(monkeypatch, memory_dir: Path) -> None:
    """Bypass the IPC / cloud-fetch / Settings dependencies of run_compile so it
    operates purely on the local temp memory_dir in local storage mode."""
    monkeypatch.setattr(scaffolder, "_load_memory_path", lambda _sid: memory_dir)
    monkeypatch.setattr(
        "ma_app.storage.fetch.fetch_session_if_missing",
        lambda mp, _sid, _settings: Path(mp),
    )
    monkeypatch.setattr(
        "ma_app.config.settings.Settings.load",
        staticmethod(lambda: SimpleNamespace(storage_mode="local")),
    )


def test_run_compile_scaffolds_when_absent(tmp_path, monkeypatch):
    memory_dir = tmp_path / "finder-nested-folders"
    _seed_session(memory_dir)
    _patch_ipc(monkeypatch, memory_dir)

    result = run_compile("test-sid", Console(quiet=True))

    assert result is not None
    assert result == memory_dir / "memory.md"
    assert result.exists()
    assert result.read_text(encoding="utf-8").strip() != ""


def test_run_compile_preserves_existing_draft(tmp_path, monkeypatch):
    memory_dir = tmp_path / "finder-nested-folders"
    _seed_session(memory_dir)
    _patch_ipc(monkeypatch, memory_dir)

    # First pass scaffolds the draft.
    result1 = run_compile("test-sid", Console(quiet=True))
    assert result1 is not None
    assert result1.exists()

    # Simulate the user editing the draft at the compile stage, then quitting
    # (Ctrl+Q) — the edited memory.md stays on disk.
    edited = "# My hand-written memory\n\nEdge case: report.pdf was locked.\n"
    result1.write_text(edited, encoding="utf-8")

    # Resuming compilation must NOT overwrite the edited draft.
    result2 = run_compile("test-sid", Console(quiet=True))

    assert result2 is not None
    assert result2 == result1
    assert result2.read_text(encoding="utf-8") == edited
