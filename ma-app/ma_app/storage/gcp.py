from __future__ import annotations

import base64
import hashlib
from pathlib import Path

from google.cloud import storage
from google.api_core.exceptions import GoogleAPIError, NotFound

from ma_app.storage.sync_worker import FileWrittenEvent

_MULTIPART_THRESHOLD = 5 * 1024 * 1024


def upload(event: FileWrittenEvent, bucket: str, project: str) -> str:
    """
    Upload a single file to GCP Cloud Storage with MD5 integrity verification.

    Upload path convention:
        gs://{bucket}/sessions/{session_id}/{memory_name}/{relative_path}

    Credentials are resolved via Application Default Credentials (ADC):
    - GOOGLE_APPLICATION_CREDENTIALS environment variable pointing to a service account JSON
    - gcloud auth application-default login
    - Compute Engine / GKE / Cloud Run service account

    For files <= 5 MB: single upload with client-computed MD5 verification.
    For files > 5 MB: chunked upload with size verification after completion.

    Returns the full GCS URI on success.
    Raises RuntimeError on upload or integrity failure.
    """
    abs_path = Path(event.abs_path)
    rel = Path(event.relative_path)

    memory_dir = abs_path
    for _ in rel.parts:
        memory_dir = memory_dir.parent
    memory_name = memory_dir.name

    blob_path = f"sessions/{event.session_id}/{memory_name}/{event.relative_path}"
    file_size = abs_path.stat().st_size

    try:
        client = storage.Client(project=project if project else None)
        bucket_obj = client.bucket(bucket)
        blob = bucket_obj.blob(blob_path)

        if file_size <= _MULTIPART_THRESHOLD:
            _upload_single(blob, abs_path, blob_path, file_size)
        else:
            _upload_chunked(blob, abs_path, blob_path, file_size)
    except GoogleAPIError as e:
        raise RuntimeError(
            f"GCP Cloud Storage upload failed for {event.relative_path}: {e}"
        ) from e

    return f"gs://{bucket}/{blob_path}"


def _upload_single(blob, abs_path: Path, blob_path: str, file_size: int) -> None:
    """
    Single-part upload. Computes local MD5, uploads with content_type,
    then verifies the stored MD5 matches.
    """
    data = abs_path.read_bytes()
    local_md5 = hashlib.md5(data).digest()
    local_md5_b64 = _base64_encode(local_md5)

    content_type = _get_content_type(abs_path)
    blob.upload_from_string(data, content_type=content_type)

    blob.reload()
    remote_md5_b64 = blob.md5_hash

    if remote_md5_b64 != local_md5_b64:
        raise RuntimeError(
            f"GCP Cloud Storage MD5 mismatch for {blob_path}: "
            f"expected {local_md5_b64}, got {remote_md5_b64}. Upload may be corrupt."
        )


def _upload_chunked(blob, abs_path: Path, blob_path: str, file_size: int) -> None:
    """
    Chunked upload for files > 5 MB. After upload verifies the blob size
    matches the local file size via blob.reload().
    """
    content_type = _get_content_type(abs_path)
    with abs_path.open("rb") as f:
        blob.upload_from_file(f, content_type=content_type, size=file_size)

    blob.reload()
    remote_size = blob.size
    if remote_size != file_size:
        raise RuntimeError(
            f"GCP Cloud Storage size mismatch for {blob_path}: "
            f"local {file_size} bytes, remote {remote_size} bytes. Upload may be corrupt."
        )


def _get_content_type(abs_path: Path) -> str:
    """Return appropriate content type based on file extension."""
    suffix = abs_path.suffix.lower()
    content_type_map = {
        ".png":  "image/png",
        ".jpg":  "image/jpeg",
        ".jpeg": "image/jpeg",
        ".webp": "image/webp",
        ".json": "application/json",
        ".md":   "text/markdown",
        ".txt":  "text/plain",
    }
    return content_type_map.get(suffix, "application/octet-stream")


def _base64_encode(data: bytes) -> str:
    """Return base64-encoded string (GCP uses base64 for MD5 hashes)."""
    return base64.b64encode(data).decode("ascii")