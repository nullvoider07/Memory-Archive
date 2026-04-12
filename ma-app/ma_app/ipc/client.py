# /Memory-Archive/ma-app/ma_app/ipc/client.py

import json
import os
import socket
from pathlib import Path
from typing import Any


# Default socket path — must match ipc::default_socket_path() in Rust.
DEFAULT_SOCKET_PATH = Path.home() / ".memory-archive" / "ma.sock"

# Custom exception for IPC-related errors.
class IPCError(Exception):
    """Raised when the IPC channel returns an error response or fails."""
    pass

# IPC client implementation
class IPCClient:
    """
    Persistent connection to the ma-core Rust IPC server.

    Designed as a context manager so the socket is always closed cleanly:

        with IPCClient() as client:
            response = client.send({"type": "ping"})

    For one-off calls without a context manager, call close() manually.
    """

    def __init__(self, socket_path: Path = DEFAULT_SOCKET_PATH) -> None:
        self.socket_path = socket_path
        self._sock: socket.socket | None = None
        self._recv_buffer: bytes = b""
        self._remote: bool = False

    # Connection management
    def connect(self) -> None:
        """
        Open the connection to ma-core.

        If Settings.ma_core_addr is set (host:port), connects via TCP and
        authenticates with the configured ipc_token. Otherwise uses the
        local Unix socket — the existing path, completely unchanged.
        """
        if self._sock is not None:
            return

        from ma_app.config.settings import Settings
        settings = Settings.load()
        addr = settings.ma_core_addr.strip()

        if addr:
            # Admin token comes from environment variable only.
            # Annotator connections use settings.annotator_key instead (set below).
            admin_token = os.environ.get("MA_IPC_TOKEN", "")
            self._connect_tcp(addr, admin_token)
        else:
            if self.socket_path == DEFAULT_SOCKET_PATH:
                self.socket_path = Path(settings.ipc_socket_path)
            self._connect_unix()

    def _connect_unix(self) -> None:
        """Connect via Unix domain socket (local mode)."""
        if not self.socket_path.exists():
            raise IPCError(
                f"ma-core is not running — socket not found at: {self.socket_path}\n"
                "Start ma-core first: cargo run -p ma-core"
            )
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            self._sock.connect(str(self.socket_path))
        except ConnectionRefusedError:
            self._sock = None
            raise IPCError(
                f"Could not connect to ma-core at: {self.socket_path}\n"
                "The socket file exists but ma-core is not accepting connections."
            )
        self._remote = False

    def _connect_tcp(self, addr: str, token: str) -> None:
        """Connect via TCP (remote mode) with TLS 1.3 + fingerprint pinning and authenticate with token."""
        import hashlib
        import ssl

        from ma_app.config.settings import Settings
        settings = Settings.load()

        try:
            host, port_str = addr.rsplit(":", 1)
            port = int(port_str)
        except ValueError:
            raise IPCError(
                f"Invalid ma_core_addr format: {addr!r}. "
                "Expected host:port e.g. 192.168.1.10:9000"
            )

        # TLS 1.3 — no chain verification (self-signed cert). The server is
        # authenticated by comparing the cert fingerprint against the pinned
        # value from settings. If no fingerprint is configured the connection
        # is still encrypted but the server is not authenticated — acceptable
        # on first connection (trust-on-first-use), but a warning is logged.
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.minimum_version = ssl.TLSVersion.TLSv1_3
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE

        raw_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        raw_sock.settimeout(5.0)
        try:
            raw_sock.connect((host, port))
        except (ConnectionRefusedError, OSError) as e:
            raw_sock.close()
            raise IPCError(
                f"Could not connect to remote ma-core at {addr}: {e}\n"
                "Check that ma-core is running and ipc_port is configured."
            ) from e

        try:
            tls_sock = ctx.wrap_socket(raw_sock, server_side=False)
        except ssl.SSLError as e:
            raw_sock.close()
            raise IPCError(
                f"TLS handshake failed connecting to {addr}: {e}\n"
                "Ensure ma-core is running with ipc_port configured and MA_IPC_TOKEN set."
            ) from e

        # Fingerprint verification — compare server cert digest against pinned value.
        cert_der = tls_sock.getpeercert(binary_form=True)
        if cert_der:
            digest = hashlib.sha256(cert_der).hexdigest().upper()
            actual_fp = ":".join(digest[i:i+2] for i in range(0, len(digest), 2))
            pinned = settings.ipc_server_fingerprint.replace(":", "").replace(" ", "").replace("\n", "").strip().upper()

            if pinned:
                if digest != pinned:
                    tls_sock.close()
                    raise IPCError(
                        f"Server certificate fingerprint mismatch — possible MITM attack.\n"
                        f"  Pinned : {settings.ipc_server_fingerprint}\n"
                        f"  Server : {actual_fp}\n"
                        "If the server cert was regenerated, run 'memory-archive tls fingerprint'\n"
                        "on the server and update: memory-archive config --ipc-server-fingerprint <fp>"
                    )
            else:
                import logging
                logging.getLogger(__name__).warning(
                    "Connecting to ma-core at %s without fingerprint verification (trust-on-first-use). "
                    "Run 'memory-archive tls fingerprint' on the server and configure: "
                    "memory-archive config --ipc-server-fingerprint %s",
                    addr, actual_fp,
                )

        tls_sock.settimeout(10.0)
        self._sock = tls_sock
        self._remote = True

        # Determine auth mode: annotator JSON or admin plaintext token.
        annotator_key = settings.annotator_key.strip()
        annotator_id = settings.annotator_id.strip()

        if annotator_key:
            if not annotator_id:
                self._sock = None
                raise IPCError(
                    "annotator_key is set but annotator_id is not configured.\n"
                    "Run: memory-archive config --annotator-id <your-name>"
                )
            auth_msg = json.dumps({
                "type": "annotator_auth",
                "annotator_id": annotator_id,
                "key": annotator_key,
            }) + "\n"
            try:
                self._sock.sendall(auth_msg.encode("utf-8"))
            except OSError as e:
                self._sock = None
                raise IPCError(f"Failed to send annotator auth: {e}") from e

            response_line = self._recv_line()
            try:
                response = json.loads(response_line)
            except json.JSONDecodeError as e:
                raise IPCError(f"Invalid annotator auth response: {response_line!r}") from e

            if response.get("type") == "error":
                self._sock = None
                raise IPCError(
                    f"Annotator authentication failed: {response.get('message', 'invalid key')}"
                )

        elif token:
            try:
                self._sock.sendall((token + "\n").encode("utf-8"))
            except OSError as e:
                self._sock = None
                raise IPCError(f"Failed to send auth token: {e}") from e

            # The server only sends a response on auth failure (AUTH_FAILED + close).
            # On success it sends nothing and enters the message loop immediately.
            # Peek with a short timeout: data means rejected, timeout means accepted.
            self._sock.settimeout(0.2)
            try:
                response_line = self._recv_line()
                response = json.loads(response_line)
                if response.get("type") == "error":
                    self._sock = None
                    raise IPCError(
                        f"Authentication failed: {response.get('message', 'invalid token')}"
                    )
            except IPCError:
                raise
            except (TimeoutError, OSError):
                # No data within 200ms — auth succeeded, server is waiting for messages.
                self._recv_buffer = b""
                pass
            except json.JSONDecodeError as e:
                raise IPCError(f"Unexpected auth response from ma-core: {e}") from e

        if self._sock is not None:
            self._sock.settimeout(None)

    # Context manager support
    def close(self) -> None:
        """Close the socket connection."""
        if self._sock is not None:
            try:
                if self._remote:
                    self._sock.unwrap()
            except OSError:
                pass
            try:
                self._sock.close()
            except OSError:
                pass
            self._sock = None

    def __enter__(self) -> "IPCClient":
        self.connect()
        return self

    def __exit__(self, *_: Any) -> None:
        self.close()

    # Messaging
    def send(self, message: dict) -> dict:
        """
        Send a message to ma-core and return the response.

        Args:
            message: A dict with at least a "type" key, e.g. {"type": "ping"}

        Returns:
            The parsed JSON response from Rust.

        Raises:
            IPCError: If not connected, send fails, or Rust returns an error.
        """
        if self._sock is None:
            raise IPCError("Not connected. Call connect() first or use as context manager.")

        # Serialise to a single JSON line terminated with newline.
        payload = json.dumps(message) + "\n"

        try:
            self._sock.sendall(payload.encode("utf-8"))
        except OSError as e:
            raise IPCError(f"Failed to send IPC message: {e}") from e

        # Read the response — a single newline-terminated JSON line.
        response_line = self._recv_line()
        try:
            response = json.loads(response_line)
        except json.JSONDecodeError as e:
            raise IPCError(f"Invalid JSON response from ma-core: {response_line!r}") from e

        # Surface Rust-side errors as Python exceptions.
        if response.get("type") == "error":
            raise IPCError(
                f"[{response.get('code', 'UNKNOWN')}] {response.get('message', 'Unknown error')}"
            )

        return response

    # Example high-level method for a specific message type.
    def ping(self) -> str:
        """
        Send a ping and return the ma-core version string.

        Returns:
            Version string from ma-core, e.g. "0.10.0"

        Raises:
            IPCError: If ma-core is unreachable or returns unexpected response.
        """
        response = self.send({"type": "ping"})
        if response.get("type") != "pong":
            raise IPCError(f"Expected pong, got: {response}")
        return response.get("version", "unknown")

    # Method to receive unsolicited pushes from ma-core (e.g. SessionDisconnected)
    def recv(self) -> dict:
        """
        Wait for the next message pushed from ma-core without sending anything.
        Used by `memory-archive start` to wait for SessionDisconnected.
        """
        if self._sock is None:
            raise IPCError("Not connected.")
        response_line = self._recv_line()
        try:
            return json.loads(response_line)
        except json.JSONDecodeError as e:
            raise IPCError(f"Invalid JSON push from ma-core: {response_line!r}") from e
        
    # Internal method to read a line from the socket
    def _recv_line(self) -> str:
        """Read exactly one newline-terminated message, buffering any remainder."""
        if self._sock is None:
            raise IPCError("Cannot receive: socket is not connected.")
        while b"\n" not in self._recv_buffer:
            chunk = self._sock.recv(4096)
            if not chunk:
                raise IPCError("ma-core closed the connection unexpectedly.")
            self._recv_buffer += chunk
        line, self._recv_buffer = self._recv_buffer.split(b"\n", 1)
        return line.decode("utf-8").strip()