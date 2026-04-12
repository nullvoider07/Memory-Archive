# /Memory-Archive/ma-app/ma_app/compiler/finalizer.py

from __future__ import annotations

import json
from pathlib import Path

from rich.console import Console

from ma_app.ipc.client import IPCClient, IPCError


def finalize_memory(session_id: str, memory_path: Path, console: Console) -> None:
    """
    T4.4 — Mark memory as fully compiled.

    1. Update metadata.json status → 'complete'.
    2. Send FinalizeMemory IPC → Rust sets Redis status → complete + 90-day TTL.
    3. Print the completion message.

    Best-effort: metadata and Redis failures are reported but do not raise.
    """
    memory_dir = memory_path.parent
    meta_path  = memory_dir / "metadata.json"

    if meta_path.exists():
        try:
            meta = json.loads(meta_path.read_text(encoding="utf-8"))
            meta["status"] = "complete"
            tmp = meta_path.with_name("metadata.json.tmp")
            tmp.write_text(
                json.dumps(meta, indent=2, ensure_ascii=False) + "\n",
                encoding="utf-8",
            )
            tmp.rename(meta_path)
        except (json.JSONDecodeError, OSError) as e:
            console.print(f"[yellow]Warning: could not update metadata.json: {e}[/yellow]")

    try:
        with IPCClient() as client:
            response = client.send({
                "type":       "finalize_memory",
                "session_id": session_id,
            })
        from ma_app.storage.sync_worker import get_worker, FileWrittenEvent
        worker = get_worker()
        if worker is not None:
            worker.enqueue(FileWrittenEvent(
                session_id=session_id,
                relative_path="metadata.json",
                abs_path=str(meta_path),
            ))
        else:
            _upload_file_to_cloud(session_id, memory_dir, "metadata.json", meta_path)
        if response.get("type") == "error":
            console.print(
                f"[yellow]Warning: Redis finalization failed: "
                f"{response.get('message', 'unknown error')}[/yellow]"
            )
    except IPCError as e:
        console.print(f"[yellow]Warning: IPC finalization failed: {e}[/yellow]")

    memory_name = memory_dir.name
    console.print(
        f"\n[green]Memory '{memory_name}' is complete.[/green]\n"
        f"  Location: {memory_path}"
    )

def _upload_file_to_cloud(session_id: str, memory_dir: Path, relative_path: str, local_path: Path) -> None:
    from ma_app.config.settings import Settings
    settings = Settings.load()
    if settings.storage_mode != "cloud_primary":
        return
    try:
        memory_name = memory_dir.name
        cloud_key = f"sessions/{session_id}/{memory_name}/{relative_path}"
        data = local_path.read_bytes()
        if settings.cloud.provider == "aws":
            import boto3
            from ma_app.storage.sync_worker import _normalize_aws_region
            region = _normalize_aws_region(settings.cloud.aws.region)
            s3 = boto3.client("s3", region_name=region)
            s3.put_object(Bucket=settings.cloud.aws.bucket, Key=cloud_key, Body=data)
        elif settings.cloud.provider == "azure":
            from azure.storage.blob import BlobServiceClient
            from azure.identity import DefaultAzureCredential
            url = f"https://{settings.cloud.azure.account}.blob.core.windows.net"
            client = BlobServiceClient(account_url=url, credential=DefaultAzureCredential())
            client.get_blob_client(container=settings.cloud.azure.container, blob=cloud_key).upload_blob(data, overwrite=True)
        elif settings.cloud.provider == "gcp":
            from google.cloud import storage as gcs
            bucket = gcs.Client().bucket(settings.cloud.gcp.bucket)
            bucket.blob(cloud_key).upload_from_string(data)
    except Exception:
        pass