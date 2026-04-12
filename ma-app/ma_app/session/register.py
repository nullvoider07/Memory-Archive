# /Memory-Archive/ma-app/ma_app/session/register.py

from __future__ import annotations
from ma_app.ipc.client import IPCClient, IPCError

# High-level session management functions that use IPCClient to interact with ma-core.
def register_session(
    mode: str,
    os_type: str,
    os_version: str,
    os_arch: str,
    os_env_id: str,
    capture_server: str,
    actuation_server: str,
    memory_name: str,
    reasoning_model_id: str | None = None,
    tenant_id: str | None = None,
) -> str:
    """
    Register a new session via IPC and return the generated session_id.

    Raises:
        IPCError: If ma-core is unreachable or registration fails.
        ValueError: If mode is not 'manual' or 'automated'.
    """
    if mode not in ("manual", "automated"):
        raise ValueError(f"mode must be 'manual' or 'automated', got: {mode!r}")

    if os_type.upper() not in ("LINUX", "WINDOWS", "MACOS"):
        raise ValueError(f"os_type must be LINUX, WINDOWS, or MACOS, got: {os_type!r}")

    message = {
        "type": "register_session",
        "mode": mode,
        "os_type": os_type.upper(),
        "os_version": os_version,
        "os_architecture": os_arch,
        "os_environment_id": os_env_id,
        "capture_server_id": capture_server,
        "actuation_server_id": actuation_server,
        "memory_name": memory_name,
        "reasoning_model_id": reasoning_model_id,
        "tenant_id": tenant_id or None,
    }

    with IPCClient() as client:
        response = client.send(message)

    if response.get("type") != "session_registered":
        raise IPCError(f"Unexpected response from ma-core: {response}")

    return response["session_id"]

# Example high-level function to fetch session status.
def get_session_status(session_id: str) -> dict:
    """
    Fetch the current status of a session from ma-core.

    Returns a dict of all session fields as stored in Redis.

    Raises:
        IPCError: If ma-core is unreachable or session does not exist.
    """
    message = {
        "type": "get_session_status",
        "session_id": session_id,
    }

    with IPCClient() as client:
        response = client.send(message)

    if response.get("type") != "session_status":
        raise IPCError(f"Unexpected response from ma-core: {response}")

    return response["session"]