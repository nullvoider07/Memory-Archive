# /Memory-Archive/ma-app/ma_app/tui/reasoning_writer.py

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from ma_app.ipc.client import IPCClient, IPCError
from ma_app.tui.session_loader import StepState, StepStatus


class ReasoningWriter:
    """
    Writes annotation state to disk and keeps Redis in sync.

    Create one instance per TUI session and reuse it throughout:

        writer = ReasoningWriter(session_data.memory_dir, session_data.session_id)

        # On Ctrl+S (save draft, no status change):
        writer.write_entry(step)

        # On Ctrl+N complete or skip (status changed, counters updated):
        writer.write_entry(step)
        writer.sync_counters(annotated_steps, skipped_steps)
    """

    def __init__(self, memory_dir: Path, session_id: str, remote_fetcher=None) -> None:
        self._memory_dir = memory_dir
        self._session_id = session_id
        self._remote_fetcher = remote_fetcher
        self._jsonl_path = memory_dir / "reasoning" / "reasoning.jsonl"
        self._meta_path  = memory_dir / "metadata.json"

    # Public API
    def write_entry(self, step: StepState) -> None:
        """
        Atomically upsert this step's entry in reasoning.jsonl.

        Strategy:
          1. Read all existing entries into a dict keyed by step_id.
          2. Build the new entry from the current StepState.
          3. Write the whole file to a .tmp sibling, then rename (atomic on POSIX).

        Safe to call multiple times for the same step — later calls overwrite
        earlier ones (idempotent by step_id).

        Silently swallows I/O errors so a disk hiccup never crashes the TUI.
        """
        try:
            self._atomic_upsert(step)
        except Exception:
            pass  # Best-effort — in-memory state is authoritative during the session

    def sync_counters(self, annotated: int, skipped: int) -> None:
        """
        Update annotated/skipped counters in metadata.json and Redis.

        Called only when a step's STATUS changes (Ctrl+N complete or skip).
        Ctrl+S (draft save) does NOT call this — counters are unchanged.

        Both operations are best-effort; failures are silently swallowed so
        the TUI stays responsive even if ma-core is unreachable.
        """
        self._update_metadata_counters(annotated, skipped)
        self._send_ipc_progress(annotated, skipped)

    # Entry building
    def _build_entry(self, step: StepState) -> dict:
        """Build the full reasoning.jsonl dict for this step."""
        return {
            "step_id":            step.step_id,
            "timestamp_action":   step.timestamp,
            "timestamp_annotated": datetime.now(timezone.utc).isoformat(),
            "action_type":        step.action_type,
            "action_subtype":     step.action_subtype,
            "raw_command":        step.raw_command,
            "converted_command":  step.converted_command,
            "image_path":         step.image_path,
            "reasoning":          step.reasoning,
            "skipped":            step.status == StepStatus.SKIPPED,
        }

    # Atomic JSONL upsert
    def _atomic_upsert(self, step: StepState) -> None:
        """Read–modify–write reasoning.jsonl with an atomic rename."""
        # Ensure parent directory exists (reasoning/ dir).
        self._jsonl_path.parent.mkdir(parents=True, exist_ok=True)

        # Read existing entries, keyed by step_id so we can upsert.
        entries: dict[int, dict] = {}
        if self._jsonl_path.exists():
            raw = self._jsonl_path.read_text(encoding="utf-8")
            for line in raw.splitlines():
                line = line.strip()
                if not line:
                    continue
                try:
                    entry = json.loads(line)
                    sid = entry.get("step_id")
                    if isinstance(sid, int):
                        entries[sid] = entry
                except json.JSONDecodeError:
                    pass  # Skip malformed lines rather than aborting

        # Insert or replace this step's entry.
        entries[step.step_id] = self._build_entry(step)

        # Write sorted by step_id to a temp file, then rename atomically.
        sorted_entries = sorted(entries.values(), key=lambda e: e["step_id"])
        content = "".join(
            json.dumps(e, ensure_ascii=False) + "\n" for e in sorted_entries
        )

        tmp = self._jsonl_path.with_name("reasoning.jsonl.tmp")
        tmp.write_text(content, encoding="utf-8")
        tmp.rename(self._jsonl_path)
        import logging
        from ma_app.storage.sync_worker import get_worker, FileWrittenEvent
        worker = get_worker()
        if worker is not None:
            worker.enqueue(FileWrittenEvent(
                session_id=self._session_id,
                relative_path="reasoning/reasoning.jsonl",
                abs_path=str(self._jsonl_path),
            ))
        elif self._remote_fetcher is not None:
            if not self._remote_fetcher.upload(
                "reasoning/reasoning.jsonl",
                self._jsonl_path,
                "application/json",
            ):
                logging.getLogger(__name__).warning(
                    "ReasoningWriter: remote upload of reasoning.jsonl failed for "
                    "session %s — annotation saved locally but not synced to cloud.",
                    self._session_id,
                )
        else:
            self._upload_to_cloud("reasoning/reasoning.jsonl", self._jsonl_path)

    # metadata.json counter update
    def _update_metadata_counters(self, annotated: int, skipped: int) -> None:
        """
        Atomically update annotated_steps and skipped_steps in metadata.json.

        The full metadata.json is preserved — only these two fields change.
        Uses the same temp+rename strategy as all other metadata writes.
        """
        if not self._meta_path.exists():
            return
        try:
            meta = json.loads(self._meta_path.read_text(encoding="utf-8"))
            meta["annotated_steps"] = annotated
            meta["skipped_steps"]   = skipped

            tmp = self._meta_path.with_name("metadata.json.tmp")
            tmp.write_text(
                json.dumps(meta, indent=2, ensure_ascii=False) + "\n",
                encoding="utf-8",
            )
            tmp.rename(self._meta_path)
            import logging
            from ma_app.storage.sync_worker import get_worker, FileWrittenEvent
            worker = get_worker()
            if worker is not None:
                worker.enqueue(FileWrittenEvent(
                    session_id=self._session_id,
                    relative_path="metadata.json",
                    abs_path=str(self._meta_path),
                ))
            elif self._remote_fetcher is not None:
                if not self._remote_fetcher.upload(
                    "metadata.json",
                    self._meta_path,
                    "application/json",
                ):
                    logging.getLogger(__name__).warning(
                        "ReasoningWriter: remote upload of metadata.json failed for "
                        "session %s.",
                        self._session_id,
                    )
            else:
                self._upload_to_cloud("metadata.json", self._meta_path)
        except (json.JSONDecodeError, OSError):
            pass  # Best-effort

    def complete_annotation(self) -> None:
        """
        Transition Redis status annotating → pending_compilation.

        Called by AnnotationScreen when the user confirms annotation is done
        (selects Compile Now or Compile Later from the completion overlay).
        Best-effort — failure is swallowed so a dead ma-core never blocks the TUI.
        """
        try:
            with IPCClient() as client:
                client.send({
                    "type":       "complete_annotation",
                    "session_id": self._session_id,
                })
        except IPCError:
            pass

    def close_session(self) -> None:
        """
        Transition Redis status annotating → pending_annotation on clean quit.

        Called by AnnotationScreen._quit_cleanly() before app.exit().
        Best-effort — failure is swallowed so a dead ma-core never blocks quit.
        """
        try:
            with IPCClient() as client:
                client.send({
                    "type":       "close_annotation",
                    "session_id": self._session_id,
                })
        except IPCError:
            pass

    # IPC progress notification
    def _send_ipc_progress(self, annotated: int, skipped: int) -> None:
        """
        Fire-and-forget IPC call to update Redis annotation counters in ma-core.

        Runs synchronously but swallows all errors — Redis lag is acceptable.
        The source of truth is metadata.json on disk; Redis is a cache.
        """
        try:
            with IPCClient() as client:
                client.send({
                    "type":       "update_annotation_progress",
                    "session_id": self._session_id,
                    "annotated":  annotated,
                    "skipped":    skipped,
                })
        except IPCError:
            pass  # ma-core may not be running during offline annotation
    
    def _upload_to_cloud(self, relative_path: str, local_path: Path) -> None:
        from ma_app.config.settings import Settings
        settings = Settings.load()
        if settings.storage_mode != "cloud_primary":
            return
        try:
            memory_name = self._memory_dir.name
            cloud_rel = f"{memory_name}/{relative_path}"
            data = local_path.read_bytes()
            if settings.cloud.provider == "aws":
                import boto3
                from ma_app.storage.sync_worker import _normalize_aws_region
                region = _normalize_aws_region(settings.cloud.aws.region)
                s3 = boto3.client("s3", region_name=region)
                s3.put_object(
                    Bucket=settings.cloud.aws.bucket,
                    Key=f"sessions/{self._session_id}/{cloud_rel}",
                    Body=data,
                )
            elif settings.cloud.provider == "azure":
                from azure.storage.blob import BlobServiceClient
                from azure.identity import DefaultAzureCredential
                credential = DefaultAzureCredential()
                url = f"https://{settings.cloud.azure.account}.blob.core.windows.net"
                client = BlobServiceClient(account_url=url, credential=credential)
                blob = client.get_blob_client(
                    container=settings.cloud.azure.container,
                    blob=f"sessions/{self._session_id}/{cloud_rel}",
                )
                blob.upload_blob(data, overwrite=True)
            elif settings.cloud.provider == "gcp":
                from google.cloud import storage as gcs
                client = gcs.Client()
                bucket = client.bucket(settings.cloud.gcp.bucket)
                blob = bucket.blob(f"sessions/{self._session_id}/{cloud_rel}")
                blob.upload_from_string(data)
        except Exception:
            pass  # Best-effort — TUI must never crash on upload failure