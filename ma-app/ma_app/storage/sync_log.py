# /Memory-Archive/ma-app/ma_app/storage/sync_log.py

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional


@dataclass
class FileRecord:
    synced: bool = False
    cloud_path: Optional[str] = None
    retry_count: int = 0
    failed_permanent: bool = False
    last_error: Optional[str] = None


class SyncLog:
    """
    Per-session sync state tracker, persisted to sync_log.json inside the
    memory directory.

    Each file that passes through the sync worker gets a FileRecord entry
    keyed by its relative path (e.g. "commands/raw_input.md").

    Lifecycle of a FileRecord:
        pending        → synced=False, retry_count=0
        upload ok      → synced=True,  cloud_path set
        upload fail    → synced=False, retry_count incremented, last_error set
        max retries    → failed_permanent=True, user alerted by SyncWorker
    """

    FILENAME = "sync_log.json"

    def __init__(self, memory_dir: Path, session_id: str) -> None:
        self._path = memory_dir / self.FILENAME
        self._session_id = session_id
        self._files: dict[str, FileRecord] = {}
        self._load()

    def mark_pending(self, relative_path: str) -> None:
        """Register a file as pending sync. No-op if already present."""
        if relative_path not in self._files:
            self._files[relative_path] = FileRecord()
            self._save()

    def mark_synced(self, relative_path: str, cloud_path: str) -> None:
        """Record a successful upload."""
        record = self._files.setdefault(relative_path, FileRecord())
        record.synced = True
        record.cloud_path = cloud_path
        record.last_error = None
        self._save()

    def mark_failed(self, relative_path: str, error: str) -> None:
        """Record a failed upload attempt, incrementing the retry counter."""
        record = self._files.setdefault(relative_path, FileRecord())
        record.synced = False
        record.retry_count += 1
        record.last_error = error
        self._save()

    def mark_permanent_failure(self, relative_path: str) -> None:
        """Mark a file as permanently failed after max retries exhausted."""
        record = self._files.setdefault(relative_path, FileRecord())
        record.failed_permanent = True
        self._save()

    def get(self, relative_path: str) -> Optional[FileRecord]:
        return self._files.get(relative_path)

    def pending_files(self) -> list[str]:
        """
        Return relative paths of all files that need a sync attempt:
        not yet synced, not permanently failed.
        Used by T5.8 resume-sync-on-restart.
        """
        return [
            path for path, rec in self._files.items()
            if not rec.synced and not rec.failed_permanent
        ]

    def failed_permanent_files(self) -> list[str]:
        """Return relative paths of all permanently failed files."""
        return [
            path for path, rec in self._files.items()
            if rec.failed_permanent
        ]

    def _load(self) -> None:
        if not self._path.exists():
            return
        try:
            data = json.loads(self._path.read_text(encoding="utf-8"))
            for rel_path, rec in data.get("files", {}).items():
                self._files[rel_path] = FileRecord(
                    synced=rec.get("synced", False),
                    cloud_path=rec.get("cloud_path"),
                    retry_count=rec.get("retry_count", 0),
                    failed_permanent=rec.get("failed_permanent", False),
                    last_error=rec.get("last_error"),
                )
        except (json.JSONDecodeError, OSError):
            pass

    def _save(self) -> None:
        import logging
        data = {
            "session_id": self._session_id,
            "files": {
                path: {
                    "synced": rec.synced,
                    "cloud_path": rec.cloud_path,
                    "retry_count": rec.retry_count,
                    "failed_permanent": rec.failed_permanent,
                    "last_error": rec.last_error,
                }
                for path, rec in self._files.items()
            },
        }
        tmp = self._path.with_suffix(".json.tmp")
        try:
            tmp.write_text(json.dumps(data, indent=2), encoding="utf-8")
            tmp.rename(self._path)
        except OSError as e:
            logging.getLogger(__name__).warning(
                "Failed to write sync_log.json for session %s: %s",
                self._session_id,
                e,
            )