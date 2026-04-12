from __future__ import annotations

import base64
import hashlib
from pathlib import Path

from azure.identity import DefaultAzureCredential
from azure.storage.blob import BlobServiceClient
from azure.core.exceptions import AzureError

from ma_app.storage.sync_worker import FileWrittenEvent

_MULTIPART_THRESHOLD = 5 * 1024 * 1024

_storage_type_cache: dict[str, str] = {}

def _detect_storage_type(account: str, credential: DefaultAzureCredential) -> str:
    """
    Detection is not performed proactively. Auto mode defaults to 'blob' and
    relies on BlobTypeNotSupported at upload time to identify true ADLS Gen2
    accounts. The DFS endpoint DNS resolves for all storage account types and
    cannot be used as a reliable HNS indicator.
    """
    return "blob"


def _resolve_storage_type(account: str, configured_type: str, credential: DefaultAzureCredential) -> str:
    """
    Resolve the effective storage type from config.
    'auto' triggers detection. Result is cached per account so the DFS probe
    only runs once per process, not once per file upload.
    """
    if configured_type != "auto":
        return configured_type
    if account in _storage_type_cache:
        return _storage_type_cache[account]
    detected = _detect_storage_type(account, credential)
    _storage_type_cache[account] = detected
    return detected


def upload(event: FileWrittenEvent, account: str, container: str, storage_type: str = "auto") -> str:
    """
    Upload a single file to Azure Storage with integrity verification.

    Supports three Azure storage backends selected via storage_type:
      blob  — Azure Blob Storage (standard StorageV2 accounts)
      adls  — Azure Data Lake Storage Gen2 (HNS-enabled accounts)
      files — Azure Files (SMB/NFS file shares)
      auto  — detect from account properties at upload time (default)

    Upload path convention (blob and adls):
        {container}/sessions/{session_id}/{memory_name}/{relative_path}

    Upload path convention (files):
        {share}/sessions/{session_id}/{memory_name}/{relative_path}

    Returns the full Azure URI on success.
    Raises RuntimeError on upload or integrity failure.
    """
    abs_path = Path(event.abs_path)
    rel = Path(event.relative_path)

    memory_dir = abs_path
    for _ in rel.parts:
        memory_dir = memory_dir.parent
    memory_name = memory_dir.name

    object_path = f"sessions/{event.session_id}/{memory_name}/{event.relative_path}"
    file_size = abs_path.stat().st_size
    credential = DefaultAzureCredential()

    effective_type = _resolve_storage_type(account, storage_type, credential)

    try:
        if effective_type == "adls":
            return _upload_adls(account, container, object_path, abs_path, file_size, credential)
        elif effective_type == "files":
            return _upload_files(account, container, object_path, abs_path, file_size, credential)
        else:
            try:
                return _upload_blob(account, container, object_path, abs_path, file_size, credential, event.relative_path)
            except AzureError as blob_err:
                if getattr(blob_err, "error_code", None) == "BlobTypeNotSupported":
                    _storage_type_cache[account] = "adls"
                    return _upload_adls(account, container, object_path, abs_path, file_size, credential)
                raise
    except AzureError as e:
        raise RuntimeError(
            f"Azure upload failed for {event.relative_path}: {e}"
        ) from e


def _upload_blob(
    account: str,
    container: str,
    blob_path: str,
    abs_path: Path,
    file_size: int,
    credential: DefaultAzureCredential,
    relative_path: str,
) -> str:
    """Upload to Azure Blob Storage (standard StorageV2)."""
    account_url = f"https://{account}.blob.core.windows.net"
    service_client = BlobServiceClient(account_url=account_url, credential=credential)
    blob_client = service_client.get_blob_client(container=container, blob=blob_path)

    if file_size <= _MULTIPART_THRESHOLD:
        _blob_upload_single(blob_client, abs_path, blob_path, file_size)
    else:
        _blob_upload_chunked(blob_client, abs_path, blob_path, file_size)

    return f"https://{account}.blob.core.windows.net/{container}/{blob_path}"


def _blob_upload_single(blob_client, abs_path: Path, blob_path: str, file_size: int) -> None:
    data = abs_path.read_bytes()
    local_md5 = hashlib.md5(data).digest()
    local_md5_b64 = base64.b64encode(local_md5).decode("ascii")

    blob_client.upload_blob(
        data,
        overwrite=True,
        content_settings=_blob_content_settings(abs_path, local_md5_b64),
    )

    props = blob_client.get_blob_properties()
    remote_md5 = props.content_settings.content_md5
    if remote_md5 is not None:
        if isinstance(remote_md5, (bytes, bytearray)):
            remote_md5 = base64.b64encode(remote_md5).decode("ascii")
        if remote_md5 != local_md5_b64:
            raise RuntimeError(
                f"Azure Blob MD5 mismatch for {blob_path}: "
                f"expected {local_md5_b64}, got {remote_md5}. Upload may be corrupt."
            )


def _blob_upload_chunked(blob_client, abs_path: Path, blob_path: str, file_size: int) -> None:
    with abs_path.open("rb") as f:
        blob_client.upload_blob(f, overwrite=True, length=file_size)

    props = blob_client.get_blob_properties()
    if props.size != file_size:
        raise RuntimeError(
            f"Azure Blob size mismatch for {blob_path}: "
            f"local {file_size} bytes, remote {props.size} bytes."
        )


def _blob_content_settings(abs_path: Path, md5_b64: str):
    from azure.storage.blob import ContentSettings

    content_type_map = {
        ".png": "image/png",
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".webp": "image/webp",
        ".json": "application/json",
        ".md": "text/markdown",
        ".txt": "text/plain",
    }
    content_type = content_type_map.get(abs_path.suffix.lower(), "application/octet-stream")
    return ContentSettings(
        content_type=content_type,
        content_md5=bytearray(base64.b64decode(md5_b64)),
    )


def _upload_adls(
    account: str,
    container: str,
    file_path: str,
    abs_path: Path,
    file_size: int,
    credential: DefaultAzureCredential,
) -> str:
    """
    Upload to Azure Data Lake Storage Gen2 (HNS-enabled account).

    Uses the azure-storage-file-datalake SDK. The container is treated as a
    file system. Intermediate directories are created automatically.
    Integrity is verified via file properties size check after upload.
    """
    from azure.storage.filedatalake import DataLakeServiceClient

    account_url = f"https://{account}.dfs.core.windows.net"
    service_client = DataLakeServiceClient(account_url=account_url, credential=credential)
    fs_client = service_client.get_file_system_client(file_system=container)

    path_parts = Path(file_path)
    directory_path = str(path_parts.parent)
    file_name = path_parts.name

    dir_client = fs_client.get_directory_client(directory_path)
    dir_client.create_directory()

    file_client = dir_client.get_file_client(file_name)

    data = abs_path.read_bytes()
    file_client.upload_data(data, overwrite=True, length=file_size)

    props = file_client.get_file_properties()
    if props.size != file_size:
        raise RuntimeError(
            f"ADLS Gen2 size mismatch for {file_path}: "
            f"local {file_size} bytes, remote {props.size} bytes."
        )

    return f"https://{account}.dfs.core.windows.net/{container}/{file_path}"


def _upload_files(
    account: str,
    share: str,
    file_path: str,
    abs_path: Path,
    file_size: int,
    credential: DefaultAzureCredential,
) -> str:
    """
    Upload to Azure Files (SMB/NFS file share).

    Uses the azure-storage-file-share SDK. The container name is treated as
    the share name. Intermediate directories are created automatically.
    Integrity is verified via file properties size check after upload.
    """
    from azure.storage.fileshare import ShareServiceClient

    account_url = f"https://{account}.file.core.windows.net"
    service_client = ShareServiceClient(account_url=account_url, credential=credential)
    share_client = service_client.get_share_client(share)

    path_parts = Path(file_path)
    directory_path = str(path_parts.parent)
    file_name = path_parts.name

    _ensure_azure_files_directory(share_client, directory_path)

    dir_client = share_client.get_directory_client(directory_path)
    file_client = dir_client.get_file_client(file_name)

    with abs_path.open("rb") as f:
        file_client.upload_file(f, length=file_size)

    props = file_client.get_file_properties()
    if props.size != file_size:
        raise RuntimeError(
            f"Azure Files size mismatch for {file_path}: "
            f"local {file_size} bytes, remote {props.size} bytes."
        )

    return f"https://{account}.file.core.windows.net/{share}/{file_path}"


def _ensure_azure_files_directory(share_client, directory_path: str) -> None:
    """
    Create all intermediate directories in the path if they do not exist.
    Azure Files requires each parent directory to be created individually.
    """
    from azure.core.exceptions import ResourceExistsError

    parts = Path(directory_path).parts
    current = ""
    for part in parts:
        current = f"{current}/{part}".lstrip("/")
        try:
            share_client.get_directory_client(current).create_directory()
        except ResourceExistsError:
            pass


def fetch_session(
    memory_dir: Path,
    session_id: str,
    memory_name: str,
    account: str,
    container: str,
    storage_type: str = "auto",
) -> None:
    """
    Download all session files from Azure Storage into memory_dir.

    Routes to the correct backend based on storage_type (auto, blob, adls, files).
    """
    credential = DefaultAzureCredential()
    effective_type = _resolve_storage_type(account, storage_type, credential)

    if effective_type == "adls":
        _fetch_adls(memory_dir, session_id, memory_name, account, container, credential)
    elif effective_type == "files":
        _fetch_files(memory_dir, session_id, memory_name, account, container, credential)
    else:
        _fetch_blob(memory_dir, session_id, memory_name, account, container, credential)


def _fetch_blob(
    memory_dir: Path,
    session_id: str,
    memory_name: str,
    account: str,
    container: str,
    credential: DefaultAzureCredential,
) -> None:
    """Download session files from Azure Blob Storage."""
    prefix = f"sessions/{session_id}/{memory_name}/"
    account_url = f"https://{account}.blob.core.windows.net"

    service_client = BlobServiceClient(account_url=account_url, credential=credential)
    container_client = service_client.get_container_client(container)

    blobs = list(container_client.list_blobs(name_starts_with=prefix))
    if not blobs:
        raise RuntimeError(
            f"Session '{session_id}' not found in Azure container '{container}' "
            f"at prefix '{prefix}'."
        )

    for blob in blobs:
        rel_path = blob.name[len(prefix):]
        if not rel_path:
            continue
        local_path = memory_dir / rel_path
        local_path.parent.mkdir(parents=True, exist_ok=True)
        blob_client = container_client.get_blob_client(blob.name)
        with local_path.open("wb") as f:
            container_client.get_blob_client(blob.name).download_blob().readinto(f)


def _fetch_adls(
    memory_dir: Path,
    session_id: str,
    memory_name: str,
    account: str,
    container: str,
    credential: DefaultAzureCredential,
) -> None:
    """Download session files from Azure Data Lake Storage Gen2."""
    from azure.storage.filedatalake import DataLakeServiceClient

    prefix = f"sessions/{session_id}/{memory_name}"
    account_url = f"https://{account}.dfs.core.windows.net"

    service_client = DataLakeServiceClient(account_url=account_url, credential=credential)
    fs_client = service_client.get_file_system_client(file_system=container)

    paths = list(fs_client.get_paths(path=prefix, recursive=True))
    files = [p for p in paths if not p.is_directory]

    if not files:
        raise RuntimeError(
            f"Session '{session_id}' not found in ADLS container '{container}' "
            f"at path '{prefix}'."
        )

    prefix_with_slash = prefix + "/"
    for path_item in files:
        rel_path = path_item.name[len(prefix_with_slash):]
        if not rel_path:
            continue
        local_path = memory_dir / rel_path
        local_path.parent.mkdir(parents=True, exist_ok=True)
        file_client = fs_client.get_file_client(path_item.name)
        with local_path.open("wb") as f:
            file_client.download_file().readinto(f)


def _fetch_files(
    memory_dir: Path,
    session_id: str,
    memory_name: str,
    account: str,
    share: str,
    credential: DefaultAzureCredential,
) -> None:
    """Download session files from Azure Files."""
    from azure.storage.fileshare import ShareServiceClient

    prefix = f"sessions/{session_id}/{memory_name}"
    account_url = f"https://{account}.file.core.windows.net"

    service_client = ShareServiceClient(account_url=account_url, credential=credential)
    share_client = service_client.get_share_client(share)
    dir_client = share_client.get_directory_client(prefix)

    files = list(_list_azure_files_recursive(dir_client, prefix))
    if not files:
        raise RuntimeError(
            f"Session '{session_id}' not found in Azure Files share '{share}' "
            f"at path '{prefix}'."
        )

    prefix_with_slash = prefix + "/"
    for file_path in files:
        rel_path = file_path[len(prefix_with_slash):]
        if not rel_path:
            continue
        local_path = memory_dir / rel_path
        local_path.parent.mkdir(parents=True, exist_ok=True)
        parts = Path(file_path)
        file_dir_client = share_client.get_directory_client(str(parts.parent))
        file_client = file_dir_client.get_file_client(parts.name)
        with local_path.open("wb") as f:
            file_client.download_file().readinto(f)


def _list_azure_files_recursive(dir_client, base_path: str):
    """Recursively yield full file paths under a directory in Azure Files."""
    for item in dir_client.list_directories_and_files():
        full_path = f"{base_path}/{item['name']}"
        if item.get("is_directory"):
            sub_dir = dir_client.get_subdirectory_client(item["name"])
            yield from _list_azure_files_recursive(sub_dir, full_path)
        else:
            yield full_path