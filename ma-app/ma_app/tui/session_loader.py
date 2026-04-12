# /Memory-Archive/ma-app/ma_app/tui/session_loader.py

from __future__ import annotations

import json
from dataclasses import dataclass, field
from enum import Enum, auto
from pathlib import Path
from typing import TYPE_CHECKING, Optional
from ma_app.ipc.client import IPCClient, IPCError

if TYPE_CHECKING:
    from ma_app.storage.remote_fetch import RemoteFetcher

# Step status
class StepStatus(Enum):
    PENDING     = auto()  # [ ] Not yet started
    IN_PROGRESS = auto()  # [~] Being edited right now (set by TUI, not loader)
    COMPLETE    = auto()  # [✓] reasoning saved
    SKIPPED     = auto()  # [-] skipped by user


# Data model
@dataclass
class StepState:
    """All the data the TUI needs for one step."""

    step_id: int
    timestamp: str          # ISO 8601 — action timestamp
    action_type: str        # "mouse" | "keyboard"
    action_subtype: str     # "left" | "right" | "type" | "press" etc.

    # Image — None if vision was disabled or the fetch failed for this step.
    image_path: Optional[str]   # Relative to memory_dir, e.g. "vision/frames/step_0001_..._at.png"
    image_fetched: bool
    marked: bool                # True for mouse steps (has red circle overlay)
    before_image_path: Optional[str] = None  # vision/frames/step_NNNN_..._before.png
    after_image_path: Optional[str] = None   # vision/frames/step_NNNN_..._after.png

    # Annotation state — populated from reasoning.jsonl on resume.
    status: StepStatus = StepStatus.PENDING
    reasoning: str = ""
    raw_command: str = ""       # Populated lazily by TUI from raw_input.md
    converted_command: str = "" # Populated lazily by TUI from converted_input.md

    # Set when this step was annotated (None if not yet done).
    timestamp_annotated: Optional[str] = None

@dataclass
class SessionState:
    """
    Complete session state passed to the TUI annotation screen.

    Built once by SessionLoader and then mutated in-place by the TUI
    as the user annotates steps.
    """

    session_id: str
    memory_dir: Path

    # Session metadata
    memory_name: str
    mode: str           # "manual" | "automated"
    total_steps: int
    annotated_steps: int
    skipped_steps: int

    # Step manifest — ordered by step_id ascending.
    steps: list[StepState] = field(default_factory=list)

    # Temp directory to clean up on exit (cloud_primary and remote modes). None in local mode.
    temp_dir: Optional[Path] = None

    # True when ma_core_addr is set — all file access goes through the IPC proxy.
    # No cloud credentials required on the annotator machine in this mode.
    is_remote: bool = False

    # RemoteFetcher instance for on-demand file access via IPC proxy (remote mode only).
    remote_fetcher: Optional["RemoteFetcher"] = None

    # 0-based index of the step the TUI should focus on when opening.
    # Points to the first non-complete, non-skipped step
    # (or the last step if the session is fully annotated).
    cursor_step: int = 0

    # True if any steps were already annotated (resume path).
    is_resume: bool = False

    # True if the session was already `annotating` in Redis when loaded
    # (meaning the TUI was killed mid-session last time).
    was_interrupted: bool = False
    claim_id: Optional[str] = None

# Exceptions
class LoadError(Exception):
    """Raised when the session cannot be loaded for annotation."""


# Loader
class SessionLoader:
    """
    Loads a completed session for TUI annotation.

    Usage:
        state = SessionLoader(session_id).load()
        # state is a fully-populated SessionState ready for the TUI
    """

    def __init__(self, session_id: str, claim_id: Optional[str] = None) -> None:
        self.session_id = session_id
        self._claim_id = claim_id

    def load(self) -> SessionState:
        """
        Full load sequence:
          1. IPC: LoadSession → get memory_path, transition Redis to annotating
          2. Detect remote mode from settings (ma_core_addr set)
          3. Remote: fetch required text files via IPC proxy into temp dir
             Local: fetch session files from cloud/disk via fetch_session_if_missing
          4. Disk: read metadata.json → build step manifest
          5. Disk: read reasoning.jsonl if present → restore annotated steps
          6. Compute cursor position and return populated SessionState

        Raises:
            LoadError: if IPC fails, session not found, wrong status,
                       metadata missing/malformed, or reasoning.jsonl corrupted.
        """
        memory_path, was_interrupted = self._ipc_load_session()

        from ma_app.config.settings import Settings
        settings = Settings.load()

        is_remote = bool(settings.ma_core_addr.strip())
        memory_name = Path(memory_path).name

        if is_remote:
            # Remote mode: annotator machine has no cloud credentials.
            # All file access is proxied through ma-core via IPC.
            # Text files needed at startup are fetched now; images are
            # fetched per-step on demand by ImageReview.
            temp_root = Path(settings.temp_session_dir) / self.session_id
            memory_dir = temp_root / memory_name
            memory_dir.mkdir(parents=True, exist_ok=True)

            from ma_app.storage.remote_fetch import RemoteFetcher
            fetcher: Optional[RemoteFetcher] = RemoteFetcher(
                session_id=self.session_id,
                img_cache_dir=temp_root / "img_cache",
            )
            self._fetch_session_files_remotely(memory_dir, fetcher)
            temp_dir: Optional[Path] = temp_root
        else:
            try:
                from ma_app.storage.fetch import fetch_session_if_missing
                local_dir = fetch_session_if_missing(memory_path, self.session_id, settings)
                memory_dir = local_dir
            except RuntimeError as e:
                raise LoadError(str(e)) from e

            temp_dir = (
                Path(settings.temp_session_dir) / self.session_id
                if settings.storage_mode == "cloud_primary"
                else None
            )
            fetcher = None

        metadata = self._read_metadata(memory_dir)
        steps = self._build_steps(metadata, memory_dir)
        existing = self._read_reasoning(memory_dir)
        annotated_count, skipped_count = self._restore_annotations(steps, existing)

        cursor = self._find_cursor(steps)
        is_resume = len(existing) > 0

        return SessionState(
            session_id=self.session_id,
            memory_dir=memory_dir,
            temp_dir=temp_dir,
            is_remote=is_remote,
            remote_fetcher=fetcher,
            memory_name=metadata.get("memory_name", ""),
            mode=metadata.get("mode", "manual"),
            total_steps=len(steps),
            annotated_steps=annotated_count,
            skipped_steps=skipped_count,
            steps=steps,
            cursor_step=cursor,
            is_resume=is_resume,
            was_interrupted=was_interrupted,
            claim_id=self._claim_id,
        )

    # IPC
    def _ipc_load_session(self) -> tuple[str, bool]:
        """
        Send LoadSession to ma-core.

        Returns:
            (memory_path, was_interrupted)
            was_interrupted is True if the Rust side reported that the session
            was already in `annotating` status (TUI crash recovery path).

        Raises:
            LoadError: if IPC fails, socket not found, or session has wrong status.
        """
        try:
            with IPCClient() as client:
                response = client.send({
                    "type": "load_session",
                    "session_id": self.session_id,
                })
        except IPCError as e:
            raise LoadError(str(e)) from e

        if response.get("type") == "error":
            code = response.get("code", "UNKNOWN")
            message = response.get("message", "")
            raise LoadError(f"[{code}] {message}")

        if response.get("type") != "session_loaded":
            raise LoadError(
                f"Unexpected IPC response when loading session: {response!r}"
            )

        memory_path: str = response["memory_path"]

        # Rust accepts `annotating` status on LoadSession (resume path).
        # We detect a prior crash by checking if reasoning.jsonl already exists
        # later in load() via is_resume. The was_interrupted flag is set here
        # only if we can infer it from the IPC response — Rust currently doesn't
        # include a field for this, so we default to False and let is_resume
        # carry the useful signal.
        was_interrupted: bool = bool(response.get("was_interrupted", False))

        return memory_path, was_interrupted

    def _fetch_session_files_remotely(self, memory_dir: Path, fetcher: "RemoteFetcher") -> None:
        """
        Fetch the minimum set of text files needed to load and display the
        session in the TUI. Images are fetched per-step on demand by ImageReview.

        Required files (LoadError raised if missing):
            metadata.json

        Optional files (silently skipped if unavailable):
            reasoning/reasoning.jsonl
            commands/converted_input.md
            commands/actuation_commands.json

        Raises:
            LoadError: if metadata.json cannot be fetched — without it the TUI
                       cannot build the step manifest and cannot open.
        """
        required = ["metadata.json"]
        optional = [
            "reasoning/reasoning.jsonl",
            "commands/converted_input.md",
            "commands/actuation_commands.json",
        ]

        for rel in required + optional:
            data = fetcher.fetch(rel)
            if data is not None:
                local = memory_dir / rel
                local.parent.mkdir(parents=True, exist_ok=True)
                local.write_bytes(data)
            elif rel in required:
                raise LoadError(
                    f"Failed to fetch '{rel}' for session '{self.session_id}' via remote proxy. "
                    "Check that ma-core is running, the session exists, and the TCP connection is configured."
                )

    # Metadata

    def _read_metadata(self, memory_dir: Path) -> dict:
        """
        Read and parse metadata.json from the memory directory.

        Raises:
            LoadError: if the file is missing or not valid JSON.
        """
        path = memory_dir / "metadata.json"
        if not path.exists():
            raise LoadError(
                f"metadata.json not found at: {path}\n"
                "Is this a valid memory directory?"
            )
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError) as e:
            raise LoadError(f"Failed to read metadata.json: {e}") from e

    def _build_steps(self, metadata: dict, memory_dir: Path) -> list[StepState]:
        """
        Build StepState list from the `steps` array in metadata.json.
        Also populates raw_command and converted_command from the command files.

        Raises:
            LoadError: if `steps` is missing or any entry is malformed.
        """
        raw_steps: list[dict] = metadata.get("steps", [])
        if not raw_steps:
            raise LoadError(
                "No steps found in metadata.json. The session may be empty, or was "
                "captured before the vision pipeline was added."
            )

        steps: list[StepState] = []
        for entry in raw_steps:
            try:
                steps.append(StepState(
                    step_id=int(entry["step_id"]),
                    timestamp=entry.get("timestamp", ""),
                    action_type=entry.get("action_type", ""),
                    action_subtype=entry.get("action_subtype", ""),
                    image_path=entry.get("image_path"),
                    image_fetched=bool(entry.get("image_fetched", False)),
                    marked=bool(entry.get("marked", False)),
                    before_image_path=entry.get("before_image_path"),
                    after_image_path=entry.get("after_image_path"),
                ))
            except (KeyError, TypeError, ValueError) as e:
                raise LoadError(
                    f"Malformed step entry in metadata.json: {entry!r} — {e}"
                ) from e

        steps.sort(key=lambda s: s.step_id)

        raw_map = self._read_actuation_commands(memory_dir, [s.step_id for s in steps])
        conv_map = self._read_converted_commands(memory_dir)

        for step in steps:
            if step.step_id in raw_map:
                step.raw_command = raw_map[step.step_id]
            if step.step_id in conv_map:
                step.converted_command = conv_map[step.step_id]

        return steps

    def _read_actuation_commands(
        self, memory_dir: Path, step_ids_ordered: list[int]
    ) -> dict[int, str]:
        """
        actuation_commands.json has no step_id field — entries are positional.
        The Nth entry corresponds to step_ids_ordered[N].
        """
        path = memory_dir / "commands" / "actuation_commands.json"
        if not path.exists():
            return {}
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
            return {
                step_id: data[i].get("raw_command", "")
                for i, step_id in enumerate(step_ids_ordered)
                if i < len(data)
            }
        except (json.JSONDecodeError, OSError):
            return {}

    def _read_converted_commands(self, memory_dir: Path) -> dict[int, str]:
        """
        converted_input.md is a markdown table with columns: Step | Timestamp | Action.
        Parse step_id from column 0 and the action string from column 2.
        Header, separator, and non-table lines are skipped automatically.
        """
        path = memory_dir / "commands" / "converted_input.md"
        if not path.exists():
            return {}
        try:
            result: dict[int, str] = {}
            for line in path.read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line.startswith("|"):
                    continue
                cols = [c.strip() for c in line.split("|")[1:-1]]
                if len(cols) < 3:
                    continue
                try:
                    step_id = int(cols[0])
                except ValueError:
                    continue
                result[step_id] = cols[2]
            return result
        except OSError:
            return {}

    # Reasoning (resume)
    def _read_reasoning(self, memory_dir: Path) -> list[dict]:
        """
        Read reasoning.jsonl if it exists.

        Returns a list of parsed dicts (one per line), or an empty list if
        the file does not exist.

        Raises:
            LoadError: if the file exists but contains invalid JSON on any line.
        """
        path = memory_dir / "reasoning" / "reasoning.jsonl"
        if not path.exists():
            return []

        entries: list[dict] = []
        try:
            raw = path.read_text(encoding="utf-8")
        except OSError as e:
            raise LoadError(f"Failed to read reasoning.jsonl: {e}") from e

        for lineno, line in enumerate(raw.splitlines(), start=1):
            line = line.strip()
            if not line:
                continue
            try:
                entries.append(json.loads(line))
            except json.JSONDecodeError as e:
                raise LoadError(
                    f"reasoning.jsonl line {lineno} is not valid JSON: {e}"
                ) from e

        return entries

    def _restore_annotations(
        self,
        steps: list[StepState],
        existing: list[dict],
    ) -> tuple[int, int]:
        """
        Apply saved reasoning entries onto the step list.

        Modifies steps in-place.
        Entries whose step_id doesn't match any step are silently ignored
        (can occur if metadata.json was partially written after a crash).

        Returns:
            (annotated_count, skipped_count)
        """
        step_index: dict[int, StepState] = {s.step_id: s for s in steps}
        annotated = 0
        skipped = 0

        for entry in existing:
            step_id = entry.get("step_id")
            if step_id is None or step_id not in step_index:
                continue

            step = step_index[step_id]
            is_skipped: bool = bool(entry.get("skipped", False))

            step.reasoning = entry.get("reasoning", "")
            step.raw_command = entry.get("raw_command", "")
            step.converted_command = entry.get("converted_command", "")
            step.timestamp_annotated = entry.get("timestamp_annotated")

            source = entry.get("source", "human")

            if is_skipped:
                step.status = StepStatus.SKIPPED
                skipped += 1
            elif source == "model" and entry.get("reasoning", "").strip():
                # VLM produced reasoning before circuit opened — pre-fill as complete.
                step.status = StepStatus.COMPLETE
                annotated += 1
            elif source == "model_degraded":
                # Step captured during degraded period — needs human annotation.
                step.status = StepStatus.PENDING
            elif entry.get("reasoning", "").strip():
                # Human or any other source with non-empty reasoning.
                step.status = StepStatus.COMPLETE
                annotated += 1
            else:
                step.status = StepStatus.PENDING

        return annotated, skipped

    # Cursor placement
    def _find_cursor(self, steps: list[StepState]) -> int:
        """
        Return the 0-based index of the step the TUI should focus on.

        - Fresh session: index 0 (first step).
        - Resume: first step that is still PENDING.
        - Fully annotated: last step index (allow review).
        """
        for i, step in enumerate(steps):
            if step.status == StepStatus.PENDING:
                return i
        return max(0, len(steps) - 1)