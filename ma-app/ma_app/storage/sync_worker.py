# /Memory-Archive/ma-app/ma_app/storage/sync_worker.py

from __future__ import annotations

import queue
import re
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from ma_app.config.settings import Settings
from ma_app.storage.sync_log import SyncLog


@dataclass
class FileWrittenEvent:
    session_id: str
    relative_path: str
    abs_path: str


_STOP_SENTINEL = object()

_BASE_BACKOFF_S = 2.0
_MAX_BACKOFF_S  = 60.0


class SyncWorker:
    """
    Receives FileWritten events from the IPC push stream and dispatches
    uploads to the configured storage backend.

    Runs on a background daemon thread so it never blocks the IPC receive
    loop in cli.py.

    Retry behaviour: exponential backoff starting at 2s, capped at 60s,
    up to sync_retry_max attempts. After max attempts the file is marked
    permanently failed and an alert is recorded.

    Alerts: permanent failures are appended to self._alerts (thread-safe).
    Call drain_alerts() at shutdown or periodically to surface them to the user.

    Usage:
        worker = SyncWorker(settings)
        worker.start()

        worker.enqueue(FileWrittenEvent(...))

        alerts = worker.drain_alerts()
        worker.stop()
    """

    def __init__(self, settings: Settings) -> None:
        self._settings = settings
        self._queue: queue.Queue = queue.Queue()
        self._thread: Optional[threading.Thread] = None
        self._logs: dict[str, SyncLog] = {}
        self._logs_lock = threading.Lock()
        self._alerts: list[str] = []
        self._alerts_lock = threading.Lock()

    def start(self) -> None:
        """Start the background worker thread."""
        if self._thread is not None and self._thread.is_alive():
            return
        self._thread = threading.Thread(
            target=self._run,
            name="sync-worker",
            daemon=True,
        )
        self._thread.start()

    def stop(self) -> None:
        """Signal the worker to drain and stop, then block until it exits."""
        self._queue.put(_STOP_SENTINEL)
        if self._thread is not None:
            self._thread.join()
            self._thread = None

    def enqueue(self, event: FileWrittenEvent) -> None:
        """Enqueue a file-written event for upload. Non-blocking."""
        self._queue.put(event)

    def drain_alerts(self) -> list[str]:
        """
        Return and clear all accumulated permanent-failure alert messages.
        Thread-safe. Call at shutdown or periodically to surface failures.
        """
        with self._alerts_lock:
            alerts = list(self._alerts)
            self._alerts.clear()
        return alerts

    def _record_alert(self, message: str) -> None:
        with self._alerts_lock:
            self._alerts.append(message)

    def _run(self) -> None:
        while True:
            item = self._queue.get()
            if item is _STOP_SENTINEL:
                self._queue.task_done()
                break
            try:
                self._dispatch(item)
            except Exception as e:
                import traceback
                self._record_alert(
                    f"Unhandled error dispatching {item.relative_path} "
                    f"(session {item.session_id}): {e}\n{traceback.format_exc()}"
                )
            finally:
                self._queue.task_done()

    def _get_log(self, event: FileWrittenEvent) -> Optional[SyncLog]:
        """
        Return the SyncLog for this event's session, creating it if needed.
        Returns None if the memory directory cannot be determined.
        """
        session_id = event.session_id
        
        with self._logs_lock:
            if session_id in self._logs:
                return self._logs[session_id]

            abs_path = Path(event.abs_path)
            rel = Path(event.relative_path)

            try:
                memory_dir = abs_path
                for _ in rel.parts:
                    memory_dir = memory_dir.parent
                if not memory_dir.is_dir():
                    self._record_alert(
                        f"Cannot locate memory directory for session {session_id} "
                        f"(derived: {memory_dir}) — sync log will not be written."
                    )
                    return None
            except Exception as e:
                self._record_alert(
                    f"Failed to derive memory_dir for session {session_id}: {e} "
                    f"— sync log will not be written."
                )
                return None

            log = SyncLog(memory_dir, session_id)
            self._logs[session_id] = log
            return log

    def _dispatch(self, event: FileWrittenEvent) -> None:
        """
        Route the event to the configured storage backend, tracking state
        in the session's sync_log.json.

        Local storage: the file is already on disk — mark synced immediately.
        Cloud backends: upload with exponential backoff retry, then mark
        synced or permanently failed.
        """
        provider = self._settings.cloud.provider if self._settings.cloud else None
        log = self._get_log(event)

        if log is not None:
            log.mark_pending(event.relative_path)

        if not provider:
            if log is not None:
                log.mark_synced(event.relative_path, event.abs_path)
            return

        max_attempts = self._settings.sync_retry_max
        last_error: Optional[Exception] = None

        for attempt in range(1, max_attempts + 1):
            try:
                cloud_path = self._upload(event, provider)
                if log is not None:
                    log.mark_synced(event.relative_path, cloud_path)
                return
            except Exception as e:
                last_error = e
                if log is not None:
                    log.mark_failed(event.relative_path, str(e))
                if attempt < max_attempts:
                    backoff = min(_BASE_BACKOFF_S * (2 ** (attempt - 1)), _MAX_BACKOFF_S)
                    time.sleep(backoff)

        if log is not None:
            log.mark_permanent_failure(event.relative_path)

        self._record_alert(
            f"PERMANENT SYNC FAILURE — {event.relative_path} "
            f"(session {event.session_id}) failed after {max_attempts} attempts. "
            f"Last error: {last_error}"
        )

    def _upload(self, event: FileWrittenEvent, provider: str) -> str:
        """
        Dispatch to the correct backend and return the cloud path on success.
        Raises on any upload failure — caller handles retry logic.
        """
        if provider == "aws":
            return self._upload_aws(event)
        elif provider == "azure":
            return self._upload_azure(event)
        elif provider == "gcp":
            return self._upload_gcp(event)
        else:
            raise ValueError(f"Unknown cloud provider: {provider!r}")

    def _upload_aws(self, event: FileWrittenEvent) -> str:
        """Upload to AWS S3."""
        from ma_app.storage.aws import upload
        cfg = self._settings.cloud.aws
        if not cfg.bucket:
            raise RuntimeError(
                "AWS bucket not configured — run: memory-archive config --aws-bucket <n>"
            )
        return upload(event, cfg.bucket, cfg.region)

    def _upload_azure(self, event: FileWrittenEvent) -> str:
        """Upload to Azure Blob Storage."""
        from ma_app.storage.azure import upload
        cfg = self._settings.cloud.azure
        if not cfg.account:
            raise RuntimeError(
                "Azure account not configured — run: memory-archive config --azure-account <n>"
            )
        if not cfg.container:
            raise RuntimeError(
                "Azure container not configured — run: memory-archive config --azure-container <n>"
            )
        return upload(event, cfg.account, cfg.container, cfg.storage_type)

    def _upload_gcp(self, event: FileWrittenEvent) -> str:
        """Upload to GCP Cloud Storage."""
        from ma_app.storage.gcp import upload
        cfg = self._settings.cloud.gcp
        if not cfg.bucket:
            raise RuntimeError(
                "GCP bucket not configured — run: memory-archive config --gcp-bucket <n>"
            )
        return upload(event, cfg.bucket, cfg.project)


_worker: Optional[SyncWorker] = None


def get_worker() -> Optional[SyncWorker]:
    """Return the active SyncWorker, or None if not initialised."""
    return _worker


def init_worker(settings: Settings) -> SyncWorker:
    """
    Initialise and start the global SyncWorker.

    Validates cloud credentials at startup so misconfiguration surfaces
    immediately rather than on the first upload attempt.

    Called once by cli.py start.
    """
    _validate_credentials(settings)
    global _worker
    _worker = SyncWorker(settings)
    _worker.start()
    return _worker


def shutdown_worker() -> None:
    """
    Stop the global SyncWorker and surface any accumulated alerts to stderr.
    Called in cli.py start finally block.
    """
    global _worker
    if _worker is not None:
        alerts = _worker.drain_alerts()
        _worker.stop()
        _worker = None
        if alerts:
            import sys
            print("\n[memory-archive] Sync failures during this session:", file=sys.stderr)
            for alert in alerts:
                print(f"  {alert}", file=sys.stderr)


_AWS_REGION_RE = re.compile(r'[a-z]{2,}-[a-z]+-[0-9]+')


def _normalize_aws_region(region: str) -> str:
    """
    Accept any format the AWS console emits and return a bare region code.

    Accepted inputs (all produce 'ap-south-2'):
      ap-south-2
      Asia Pacific (Hyderabad) ap-south-2
    """
    if not region:
        return region
    m = _AWS_REGION_RE.search(region)
    return m.group(0) if m else region.strip()


def _validate_credentials(settings: Settings) -> None:
    """
    Validate that cloud credentials are reachable before starting the worker.
    Raises RuntimeError with a clear message if validation fails.
    Only runs if a cloud provider is configured.
    """
    provider = settings.cloud.provider if settings.cloud else None
    if not provider:
        return

    if provider == "aws":
        _validate_aws(settings)
    elif provider == "azure":
        _validate_azure(settings)
    elif provider == "gcp":
        _validate_gcp(settings)


def _validate_aws(settings: Settings) -> None:
    """
    Validate AWS configuration at startup.

    local mode: verify credentials are resolvable only. Bucket errors are
    caught at upload time and tracked in sync_log.json — this is intentional
    and allows T5.3a-style failure simulation testing.

    cloud_primary mode: verify both credentials and bucket accessibility.
    There is no local disk fallback in this mode — a bad bucket means data
    loss, so a hard fail at startup is required.
    """
    cfg = settings.cloud.aws

    if not cfg.bucket:
        raise RuntimeError(
            "AWS provider selected but no bucket configured. "
            "Run: memory-archive config --aws-bucket <n>"
        )

    try:
        import boto3
        from botocore.exceptions import NoCredentialsError, ClientError

        region = _normalize_aws_region(cfg.region) or None

        if settings.storage_mode == "cloud_primary":
            client = boto3.client("s3", region_name=region)
            client.head_bucket(Bucket=cfg.bucket)
        else:
            sts = boto3.client("sts")
            sts.get_caller_identity()
    except Exception as e:
        from botocore.exceptions import NoCredentialsError, ClientError
        if isinstance(e, NoCredentialsError):
            raise RuntimeError(
                "AWS credentials not found. Set AWS_ACCESS_KEY_ID / "
                "AWS_SECRET_ACCESS_KEY, configure ~/.aws/credentials, "
                "or assign an IAM role."
            ) from e
        if isinstance(e, ClientError):
            code = e.response.get("Error", {}).get("Code", "")
            if code == "404":
                raise RuntimeError(
                    f"AWS S3 bucket '{cfg.bucket}' does not exist "
                    f"in region '{cfg.region}'."
                ) from e
            if code in ("403", "AccessDenied"):
                raise RuntimeError(
                    f"AWS credentials found but access to bucket '{cfg.bucket}' "
                    f"is denied. Check IAM permissions (s3:PutObject, s3:HeadBucket)."
                ) from e
        raise RuntimeError(f"AWS credential check failed: {e}") from e


def _validate_azure(settings: Settings) -> None:
    """
    Validate Azure configuration at startup.

    local mode: verify credentials are resolvable only. Container errors are
    caught at upload time and tracked in sync_log.json.

    cloud_primary mode: verify both credentials and container accessibility.
    There is no local disk fallback in this mode so a hard fail is required.
    """
    cfg = settings.cloud.azure

    if not cfg.account:
        raise RuntimeError(
            "Azure provider selected but no storage account configured. "
            "Run: memory-archive config --azure-account <n>"
        )
    if not cfg.container:
        raise RuntimeError(
            "Azure provider selected but no container configured. "
            "Run: memory-archive config --azure-container <n>"
        )

    try:
        from azure.identity import DefaultAzureCredential
        from azure.storage.blob import BlobServiceClient
        from azure.core.exceptions import ResourceNotFoundError, ClientAuthenticationError

        credential = DefaultAzureCredential()
        account_url = f"https://{cfg.account}.blob.core.windows.net"
        service_client = BlobServiceClient(account_url=account_url, credential=credential)

        if settings.storage_mode == "cloud_primary":
            service_client.get_container_client(cfg.container).get_container_properties()
        else:
            credential.get_token("https://storage.azure.com/.default")
    except Exception as e:
        from azure.core.exceptions import ResourceNotFoundError, ClientAuthenticationError
        if isinstance(e, ClientAuthenticationError):
            raise RuntimeError(
                "Azure credentials not found or invalid. Set AZURE_CLIENT_ID / "
                "AZURE_CLIENT_SECRET / AZURE_TENANT_ID, use a managed identity, "
                "or run 'az login'."
            ) from e
        if isinstance(e, ResourceNotFoundError):
            raise RuntimeError(
                f"Azure container '{cfg.container}' does not exist in account '{cfg.account}'."
            ) from e
        raise RuntimeError(f"Azure credential validation failed: {e}") from e


def _validate_gcp(settings: Settings) -> None:
    """
    Verify GCP credentials are resolvable and the configured bucket exists.
    Raises RuntimeError on any misconfiguration.
    """
    cfg = settings.cloud.gcp

    if not cfg.bucket:
        raise RuntimeError(
            "GCP provider selected but no bucket configured. "
            "Run: memory-archive config --gcp-bucket <n>"
        )

    try:
        from google.cloud import storage
        from google.api_core.exceptions import NotFound, Forbidden, Unauthenticated
        from google.auth.exceptions import DefaultCredentialsError

        client = storage.Client(project=cfg.project if cfg.project else None)
        bucket = client.bucket(cfg.bucket)
        bucket.reload()
    except Exception as e:
        from google.api_core.exceptions import NotFound, Forbidden, Unauthenticated
        from google.auth.exceptions import DefaultCredentialsError
        if isinstance(e, DefaultCredentialsError):
            raise RuntimeError(
                "GCP credentials not found. Set GOOGLE_APPLICATION_CREDENTIALS "
                "environment variable to point to a service account JSON file, "
                "or run 'gcloud auth application-default login'."
            ) from e
        if isinstance(e, (Unauthenticated, Forbidden)):
            raise RuntimeError(
                "GCP credentials found but access to bucket "
                f"'{cfg.bucket}' is denied. Check IAM permissions "
                "(storage.objects.create, storage.buckets.get)."
            ) from e
        if isinstance(e, NotFound):
            raise RuntimeError(
                f"GCP Cloud Storage bucket '{cfg.bucket}' does not exist."
            ) from e
        raise RuntimeError(f"GCP credential validation failed: {e}") from e