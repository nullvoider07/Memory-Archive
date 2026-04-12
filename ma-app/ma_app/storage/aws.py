# /Memory-Archive/ma-app/ma_app/storage/aws.py

from __future__ import annotations

import hashlib
import boto3
import re
from pathlib import Path
from botocore.exceptions import BotoCoreError, ClientError
from ma_app.storage.sync_worker import FileWrittenEvent
from ma_app.storage.sync_worker import FileWrittenEvent, _normalize_aws_region

_MULTIPART_THRESHOLD = 5 * 1024 * 1024

def upload(event: FileWrittenEvent, bucket: str, region: str) -> str:
    """
    Upload a single file to AWS S3 with ETag integrity verification.

    Upload path convention:
        s3://{bucket}/sessions/{session_id}/{memory_name}/{relative_path}

    The memory_name is derived from abs_path by stripping the relative_path
    parts — the remaining leaf directory is the memory directory name.

    For files <= 5 MB: single-part upload, ETag verified against local MD5.
    For files > 5 MB: boto3 managed multipart upload. ETag is a composite
    hash (MD5 of part MD5s) so we verify the object exists and its size
    matches rather than the ETag directly.

    Returns the full S3 URI on success.
    Raises RuntimeError on upload or integrity failure.
    """
    abs_path = Path(event.abs_path)
    rel = Path(event.relative_path)

    memory_dir = abs_path
    for _ in rel.parts:
        memory_dir = memory_dir.parent
    memory_name = memory_dir.name

    s3_key = f"sessions/{event.session_id}/{memory_name}/{event.relative_path}"
    file_size = abs_path.stat().st_size

    client = boto3.client("s3", region_name=_normalize_aws_region(region) or None)

    try:
        if file_size <= _MULTIPART_THRESHOLD:
            _upload_single(client, abs_path, bucket, s3_key, file_size)
        else:
            _upload_multipart(client, abs_path, bucket, s3_key, file_size)
    except (BotoCoreError, ClientError) as e:
        raise RuntimeError(
            f"S3 upload failed for {event.relative_path}: {e}"
        ) from e

    return f"s3://{bucket}/{s3_key}"


def _upload_single(
    client,
    abs_path: Path,
    bucket: str,
    s3_key: str,
    file_size: int,
) -> None:
    """
    Single-part upload. Computes local MD5, uploads, then verifies the
    S3 ETag matches. S3 returns MD5 as ETag for single-part uploads.
    """
    data = abs_path.read_bytes()
    local_md5 = hashlib.md5(data).hexdigest()

    response = client.put_object(
        Bucket=bucket,
        Key=s3_key,
        Body=data,
        ContentMD5=_base64_md5(data),
    )

    etag = response.get("ETag", "").strip('"')
    if etag and etag != local_md5:
        raise RuntimeError(
            f"S3 ETag mismatch for {s3_key}: "
            f"expected {local_md5}, got {etag}. Upload may be corrupt."
        )


def _upload_multipart(
    client,
    abs_path: Path,
    bucket: str,
    s3_key: str,
    file_size: int,
) -> None:
    """
    Multipart upload for files > 5 MB. After upload, verifies the object
    exists in S3 and its size matches the local file size.
    """
    client.upload_file(
        Filename=str(abs_path),
        Bucket=bucket,
        Key=s3_key,
    )

    head = client.head_object(Bucket=bucket, Key=s3_key)
    remote_size = head.get("ContentLength", -1)
    if remote_size != file_size:
        raise RuntimeError(
            f"S3 size mismatch for {s3_key}: "
            f"local {file_size} bytes, remote {remote_size} bytes. "
            f"Upload may be corrupt."
        )


def _base64_md5(data: bytes) -> str:
    """Return the base64-encoded MD5 of data, required by S3 ContentMD5 header."""
    import base64
    return base64.b64encode(hashlib.md5(data).digest()).decode("ascii")