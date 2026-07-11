# /Memory-Archive/ma-app/ma_app/compiler/scaffolder.py

from __future__ import annotations

import json
from pathlib import Path
from typing import Optional

from rich.console import Console

from ma_app.ipc.client import IPCClient, IPCError


class CompileError(Exception):
    """Raised when the session cannot be loaded for compilation."""


def run_compile(session_id: str, console: Console) -> Optional[Path]:
    """
    Orchestrate the full T4.2 scaffold:
      1. Load memory_path from Redis via IPC (validates status is pending_compilation)
      2. Read metadata.json + reasoning.jsonl
      3. Generate and write memory.md scaffold
      4. Return the path to memory.md for T4.3 to open in the editor

    Returns the Path to memory.md on success, or None on error.
    Called by:
      - cli.py compile command (standalone later path)
      - annotation.py _on_annotation_complete() (compile now path)
    """
    try:
        memory_dir = _load_memory_path(session_id)
    except CompileError as e:
        console.print(f"[red]Cannot compile: {e}[/red]")
        return None

    try:
        from ma_app.storage.fetch import fetch_session_if_missing
        from ma_app.config.settings import Settings
        settings = Settings.load()
        memory_dir = fetch_session_if_missing(str(memory_dir), session_id, settings)
    except RuntimeError as e:
        console.print(f"[red]Failed to fetch session files: {e}[/red]")
        return None

    try:
        metadata = _read_metadata(memory_dir)
    except CompileError as e:
        console.print(f"[red]metadata.json error: {e}[/red]")
        return None

    try:
        entries = _read_reasoning(memory_dir)
    except CompileError as e:
        console.print(f"[red]reasoning.jsonl error: {e}[/red]")
        return None

    memory_md_path = memory_dir / "memory.md"

    # Preserve an existing draft. A session left at pending_compilation by a Ctrl+Q
    # quit keeps its memory.md (with the notes/edge-cases already written), so
    # reopen it rather than regenerating — resuming compilation must be lossless.
    # Delete memory.md to force a fresh scaffold.
    if memory_md_path.exists():
        console.print(
            f"[green]Resuming existing memory.md draft:[/green] {memory_md_path}\n"
            f"  (delete it to regenerate a fresh scaffold)"
        )
        return memory_md_path

    scaffold = generate_scaffold(metadata, entries)

    try:
        tmp = memory_md_path.with_suffix(".md.tmp")
        tmp.write_text(scaffold, encoding="utf-8")
        tmp.rename(memory_md_path)
        from ma_app.config.settings import Settings
        settings = Settings.load()
        if settings.storage_mode == "cloud_primary":
            from ma_app.compiler.finalizer import _upload_file_to_cloud
            _upload_file_to_cloud(session_id, memory_dir, "memory.md", memory_md_path)
    except OSError as e:
        console.print(f"[red]Failed to write memory.md: {e}[/red]")
        return None

    annotated = sum(1 for e in entries if not e.get("skipped", False))
    skipped   = sum(1 for e in entries if e.get("skipped", False))
    console.print(
        f"[green]Scaffold written:[/green] {memory_md_path}\n"
        f"  Steps: {metadata.get('total_steps', len(entries))}  "
        f"Annotated: {annotated}  Skipped: {skipped}"
    )

    return memory_md_path


def generate_scaffold(metadata: dict, entries: list[dict]) -> str:
    """
    Build the full memory.md scaffold string.

    `metadata` — parsed metadata.json dict.
    `entries`  — list of parsed reasoning.jsonl dicts, ordered by step_id.

    Returns the complete markdown string ready to write to disk.
    """
    memory_name = metadata.get("memory_name", "untitled")
    lines: list[str] = []

    lines.append(f"# Memory: {memory_name}")
    lines.append("")
    lines.append("## Overview")
    lines.append("<!-- Describe what this memory does and when to use it -->")
    lines.append("")
    lines.append("## Prerequisites")
    lines.append("<!-- List any requirements before running this task -->")
    lines.append("")
    lines.append("## Steps")
    lines.append("")

    entry_by_step: dict[int, dict] = {
        e["step_id"]: e for e in entries if "step_id" in e
    }

    steps = metadata.get("steps", [])
    if not steps:
        steps = [{"step_id": e["step_id"]} for e in entries]

    for step in sorted(steps, key=lambda s: s.get("step_id", 0)):
        step_id = step.get("step_id", 0)
        entry   = entry_by_step.get(step_id)

        converted = (
            entry.get("converted_command", "")
            if entry
            else step.get("action_type", "") + "/" + step.get("action_subtype", "")
        )
        heading = f"### Step {step_id} — {converted}"
        lines.append(heading)
        lines.append("")

        if entry is None or entry.get("skipped", False):
            lines.append("<!-- No reasoning provided -->")
        else:
            reasoning = (entry.get("reasoning") or "").strip()
            source = entry.get("source", "human")
            if reasoning:
                lines.append(reasoning)
            elif source == "model_degraded":
                lines.append("<!-- VLM reasoning unavailable — circuit breaker was open during capture. Please annotate manually. -->")
            else:
                lines.append("<!-- No reasoning provided -->")

        image_path = (entry or {}).get("image_path") or step.get("image_path")
        if image_path:
            lines.append("")
            lines.append(f"![Step {step_id}]({image_path})")

        lines.append("")

    lines.append("## Notes & Edge Cases")
    lines.append(
        "<!-- Known failure modes, alternative paths, "
        "environment-specific behavior -->"
    )
    lines.append("")

    return "\n".join(lines)


def _load_memory_path(session_id: str) -> Path:
    """
    Fetch memory_path from Redis via GetSessionStatus IPC.

    Validates that the session status is pending_compilation.

    Raises:
        CompileError: if IPC fails or session is in wrong status.
    """
    try:
        with IPCClient() as client:
            response = client.send({
                "type":       "get_session_status",
                "session_id": session_id,
            })
    except IPCError as e:
        raise CompileError(str(e)) from e

    if response.get("type") == "error":
        raise CompileError(response.get("message", "IPC error"))

    if response.get("type") != "session_status":
        raise CompileError(f"Unexpected IPC response: {response!r}")

    session = response.get("session", {})
    status  = session.get("status", "")

    if status not in ("pending_compilation", "annotating"):
        raise CompileError(
            f"Session status is '{status}' — expected 'pending_compilation'.\n"
            "Run annotation to completion before compiling."
        )

    memory_path = session.get("memory_path", "")
    if not memory_path:
        raise CompileError("Session record has no memory_path in Redis.")

    try:
        from ma_app.storage.fetch import fetch_session_if_missing
        from ma_app.config.settings import Settings
        local_path = fetch_session_if_missing(memory_path, session_id, Settings.load())
    except RuntimeError as e:
        raise CompileError(str(e)) from e

    if not local_path.exists():
        raise CompileError(f"Memory directory not found: {local_path}")

    return local_path


def _read_metadata(memory_dir: Path) -> dict:
    """
    Read and parse metadata.json.

    Raises:
        CompileError: if the file is missing or invalid JSON.
    """
    path = memory_dir / "metadata.json"
    if not path.exists():
        raise CompileError(f"metadata.json not found at: {path}")
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as e:
        raise CompileError(f"Failed to read metadata.json: {e}") from e


def _read_reasoning(memory_dir: Path) -> list[dict]:
    """
    Read and parse reasoning.jsonl.

    Returns an empty list if the file does not exist.

    Raises:
        CompileError: if any line is invalid JSON.
    """
    path = memory_dir / "reasoning" / "reasoning.jsonl"
    if not path.exists():
        return []

    entries: list[dict] = []
    try:
        raw = path.read_text(encoding="utf-8")
    except OSError as e:
        raise CompileError(f"Failed to read reasoning.jsonl: {e}") from e

    for lineno, line in enumerate(raw.splitlines(), start=1):
        line = line.strip()
        if not line:
            continue
        try:
            entries.append(json.loads(line))
        except json.JSONDecodeError as e:
            raise CompileError(
                f"reasoning.jsonl line {lineno} is not valid JSON: {e}"
            ) from e

    entries.sort(key=lambda e: e.get("step_id", 0))
    return entries