# /Memory-Archive/ma-app/ma_app/storage/remote_fetch.py

from __future__ import annotations

import base64
import threading
from pathlib import Path
from typing import Optional

from ma_app.ipc.client import IPCClient, IPCError


class RemoteFetcher:
    """
    Proxy client for session file access via the ma-core IPC server.

    Used by remote annotators who have no direct cloud storage access.
    All file reads and writes are routed through ma-core, which holds
    the cloud credentials.

    Thread-safe: each operation opens a fresh IPC connection so multiple
    threads (e.g. background prefetch) can run concurrently without
    sharing a socket.
    """

    def __init__(self, session_id: str, img_cache_dir: Path) -> None:
        self.session_id = session_id
        self.img_cache_dir = img_cache_dir
        self.img_cache_dir.mkdir(parents=True, exist_ok=True)

    def fetch(self, relative_path: str) -> Optional[bytes]:
        """
        Fetch a single session file via IPC proxy.

        Returns raw bytes on success, None on any error. Errors are
        swallowed silently — the caller decides whether absence is fatal.
        """
        try:
            with IPCClient() as client:
                response = client.send({
                    "type": "fetch_file",
                    "session_id": self.session_id,
                    "relative_path": relative_path,
                })
            if response.get("type") == "file_data":
                encoded = response.get("bytes", "")
                if encoded:
                    return base64.b64decode(encoded)
        except IPCError:
            pass
        except Exception as e:
            import logging
            logging.getLogger(__name__).warning(
                "RemoteFetcher.fetch: unexpected error fetching '%s': %s",
                relative_path, e,
            )
        return None

    def fetch_and_cache_image(self, relative_path: str) -> Optional[Path]:
        """
        Fetch an image file and write it to the local image cache.

        Returns the local cached path on success, None on failure.
        If the image is already cached, returns the existing path immediately
        without making an IPC call.
        """
        import logging
        rel = Path(relative_path)
        if rel.is_absolute() or ".." in rel.parts:
            logging.getLogger(__name__).warning(
                "RemoteFetcher.fetch_and_cache_image: rejected unsafe path: %r",
                relative_path,
            )
            return None
        local = self.img_cache_dir / rel
        if local.exists():
            return local
        data = self.fetch(relative_path)
        if data is None:
            return None
        local.parent.mkdir(parents=True, exist_ok=True)
        local.write_bytes(data)
        return local

    def prefetch_step_images(self, image_paths: list[str]) -> None:
        """
        Kick off a background fetch for a list of image paths.

        Returns immediately. Images are written to the cache in a daemon
        thread. Calling code should check the cache before using images.
        No-op for paths already present in the cache.
        """
        def _run() -> None:
            for rel in image_paths:
                if rel and not (self.img_cache_dir / rel).exists():
                    self.fetch_and_cache_image(rel)

        threading.Thread(target=_run, daemon=True).start()

    def upload(
        self,
        relative_path: str,
        local_path: Path,
        content_type: str = "application/octet-stream",
    ) -> bool:
        """
        Upload a local file to session storage via the ma-core proxy.

        Used by remote annotators to persist reasoning.jsonl and metadata.json
        without requiring cloud credentials on the annotator machine. ma-core
        writes the bytes to cloud storage using its own credentials.

        Returns True on success, False on any error.
        """
        try:
            data = local_path.read_bytes()
            encoded = base64.b64encode(data).decode("ascii")
            with IPCClient() as client:
                response = client.send({
                    "type": "upload_file",
                    "session_id": self.session_id,
                    "relative_path": relative_path,
                    "bytes": encoded,
                    "content_type": content_type,
                })
            return response.get("type") == "file_uploaded"
        except (IPCError, Exception):
            return False

    def list_files(self, prefix: str = "") -> list[str]:
        """
        List session files, optionally filtered by path prefix.

        Returns a list of relative paths. Returns an empty list on error.
        """
        try:
            with IPCClient() as client:
                response = client.send({
                    "type": "list_session_files",
                    "session_id": self.session_id,
                    "prefix": prefix,
                })
            if response.get("type") == "session_file_list":
                return [f["path"] for f in response.get("files", [])]
        except (IPCError, Exception):
            pass
        return []