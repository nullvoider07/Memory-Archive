# /Memory-Archive/ma-app/ma_app/storage/fetch.py

from __future__ import annotations

from pathlib import Path

from ma_app.config.settings import Settings
from ma_app.storage.sync_worker import _normalize_aws_region


def fetch_session_if_missing(memory_path: str, session_id: str, settings: Settings) -> Path:
    """
    Ensure the session's memory directory exists on local disk.

    In local mode: if the directory already exists at memory_path, returns it
    immediately. If missing, downloads from cloud to memory_path.

    In cloud_primary mode: always downloads to a temp directory under
    temp_session_dir/{session_id}/{memory_name}/ so the annotator machine
    never writes to the server's storage path. Returns the temp path.

    Called by SessionLoader.load() and scaffolder._load_memory_path() after
    receiving memory_path from Redis, before any local disk reads.

    Args:
        memory_path: Absolute local path to the memory directory (from Redis).
        session_id:  Session ID, used to construct the cloud key prefix.
        settings:    Loaded Settings instance.

    Returns:
        Path to the local memory directory (may differ from memory_path in
        cloud_primary mode).

    Raises:
        RuntimeError: If cloud provider is configured but download fails.
        RuntimeError: If no cloud provider is configured and directory is missing.
    """
    memory_name = Path(memory_path).name

    if settings.storage_mode == "cloud_primary":
        # In cloud_primary mode download to a temp directory — the original
        # memory_path is on the capture server and is not accessible here.
        temp_dir = Path(settings.temp_session_dir) / session_id / memory_name
        if temp_dir.exists():
            return temp_dir
        provider = settings.cloud.provider if settings.cloud else None
        if not provider:
            raise RuntimeError(
                "storage_mode is cloud_primary but no cloud provider is configured. "
                "Run: memory-archive config --cloud <aws|azure|gcp>"
            )
        temp_dir.mkdir(parents=True, exist_ok=True)
        if provider == "aws":
            _fetch_aws(temp_dir, session_id, memory_name, settings)
        elif provider == "azure":
            _fetch_azure(temp_dir, session_id, memory_name, settings)
        elif provider == "gcp":
            _fetch_gcp(temp_dir, session_id, memory_name, settings)
        else:
            raise RuntimeError(f"Unknown cloud provider: {provider!r}")
        return temp_dir

    # local mode — download to memory_path if missing
    memory_dir = Path(memory_path)
    if memory_dir.exists():
        return memory_dir

    provider = settings.cloud.provider if settings.cloud else None
    if not provider:
        raise RuntimeError(
            f"Session directory not found locally: {memory_dir}\n"
            "No cloud provider is configured, so files cannot be fetched remotely.\n"
            "If this session was captured on another machine, configure a cloud "
            "provider: memory-archive config --cloud <aws|azure|gcp>"
        )

    memory_name = memory_dir.name
    if provider == "aws":
        _fetch_aws(memory_dir, session_id, memory_name, settings)
    elif provider == "azure":
        _fetch_azure(memory_dir, session_id, memory_name, settings)
    elif provider == "gcp":
        _fetch_gcp(memory_dir, session_id, memory_name, settings)
    else:
        raise RuntimeError(f"Unknown cloud provider: {provider!r}")
    return memory_dir


def _fetch_aws(
    memory_dir: Path,
    session_id: str,
    memory_name: str,
    settings: Settings,
) -> None:
    """Download all session files from AWS S3."""
    import boto3
    from botocore.exceptions import ClientError

    cfg = settings.cloud.aws
    if not cfg.bucket:
        raise RuntimeError(
            "AWS bucket not configured — run: memory-archive config --aws-bucket <name>"
        )

    prefix = f"sessions/{session_id}/{memory_name}/"
    client = boto3.client("s3", region_name=_normalize_aws_region(cfg.region) or None)

    try:
        paginator = client.get_paginator("list_objects_v2")
        pages = paginator.paginate(Bucket=cfg.bucket, Prefix=prefix)

        found_any = False
        for page in pages:
            for obj in page.get("Contents", []):
                key = obj["Key"]
                rel_path = key[len(prefix):]
                if not rel_path:
                    continue
                found_any = True
                local_path = memory_dir / rel_path
                local_path.parent.mkdir(parents=True, exist_ok=True)
                client.download_file(cfg.bucket, key, str(local_path))

        if not found_any:
            raise RuntimeError(
                f"Session '{session_id}' not found in S3 bucket '{cfg.bucket}' "
                f"at prefix '{prefix}'."
            )
    except ClientError as e:
        raise RuntimeError(f"S3 fetch failed for session '{session_id}': {e}") from e


def _fetch_azure(
    memory_dir: Path,
    session_id: str,
    memory_name: str,
    settings: Settings,
) -> None:
    """Download all session files from Azure Storage (blob, adls, or files)."""
    from azure.core.exceptions import AzureError
    from ma_app.storage.azure import fetch_session

    cfg = settings.cloud.azure
    if not cfg.account:
        raise RuntimeError(
            "Azure account not configured — run: memory-archive config --azure-account <name>"
        )
    if not cfg.container:
        raise RuntimeError(
            "Azure container not configured — run: memory-archive config --azure-container <name>"
        )

    try:
        fetch_session(
            memory_dir=memory_dir,
            session_id=session_id,
            memory_name=memory_name,
            account=cfg.account,
            container=cfg.container,
            storage_type=cfg.storage_type,
        )
    except AzureError as e:
        raise RuntimeError(
            f"Azure fetch failed for session '{session_id}': {e}"
        ) from e


def _fetch_gcp(
    memory_dir: Path,
    session_id: str,
    memory_name: str,
    settings: Settings,
) -> None:
    """Download all session files from GCP Cloud Storage."""
    from google.cloud import storage
    from google.api_core.exceptions import GoogleAPIError

    cfg = settings.cloud.gcp
    if not cfg.bucket:
        raise RuntimeError(
            "GCP bucket not configured — run: memory-archive config --gcp-bucket <name>"
        )

    prefix = f"sessions/{session_id}/{memory_name}/"

    try:
        client = storage.Client(project=cfg.project if cfg.project else None)
        bucket = client.bucket(cfg.bucket)

        blobs = list(client.list_blobs(bucket, prefix=prefix))
        if not blobs:
            raise RuntimeError(
                f"Session '{session_id}' not found in GCP bucket '{cfg.bucket}' "
                f"at prefix '{prefix}'."
            )

        for blob in blobs:
            rel_path = blob.name[len(prefix):]
            if not rel_path:
                continue
            local_path = memory_dir / rel_path
            local_path.parent.mkdir(parents=True, exist_ok=True)
            blob.download_to_filename(str(local_path))
    except GoogleAPIError as e:
        raise RuntimeError(
            f"GCP fetch failed for session '{session_id}': {e}"
        ) from e