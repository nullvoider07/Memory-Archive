"""Local full-stack harness for the Control-Center compatibility matrix.

Brings up a real `control-center-server` (one specific release) and a real
`ma-core` against it, then drives sessions through the real `memory-archive`
CLI. Nothing is mocked: the point of these tests is the wire behaviour between
two independently versioned binaries, which a mock cannot tell us anything
about.

Isolation, in order of how much damage the alternative would do:

- **Redis DB 15, never DB 0.** DB 0 holds the live session registry. A test that
  wrote there could destroy recorded corpus sessions. Every helper here pins the
  DB index, and `reset_registry()` refuses to run against any other one.
- Ephemeral loopback ports, so a running production server is never contended.
- Storage, config and IPC socket under a per-test temporary directory.
"""
from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import shutil
import socket
import subprocess
import time
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Generator, Optional

REPO_ROOT = Path(__file__).resolve().parent.parent

# DB 0 is the live registry. Tests must never touch it.
REDIS_TEST_DB = 15
JWT_SECRET = "integration-test-secret-0123456789abcdef"

# Where to find per-version Control-Center servers. CI populates this; locally a
# missing version simply skips its parameterisation rather than failing.
#   <dir>/1.0.0/control-center-server
#   <dir>/1.2.0/control-center-server
CC_BIN_DIR = Path(os.environ.get("MA_TEST_CC_BIN_DIR", "/tmp/ma-test-cc"))


def cc_server_binary(version: str) -> Optional[Path]:
    """Return the server binary for `version`, or None when it is not staged."""
    candidate = CC_BIN_DIR / version / "control-center-server"
    return candidate if candidate.is_file() else None


def cc_agent_binary(version: str) -> Optional[Path]:
    """Return the agent binary for `version`, or None when it is not staged.

    Staged from the same archive as the server, so a version has both or neither
    — except for releases published before the archive carried an agent, which is
    why this is a separate lookup rather than an assumption.
    """
    candidate = CC_BIN_DIR / version / "control-center-agent"
    return candidate if candidate.is_file() else None


def ma_core_binary() -> Optional[Path]:
    for profile in ("debug", "release"):
        candidate = REPO_ROOT / "target" / profile / "ma-core"
        if candidate.is_file():
            return candidate
    return None


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def wait_port(port: int, timeout: float = 20.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(0.5)
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return True
        time.sleep(0.2)
    return False


def wait_for_file_line(path: Path, needle: str, timeout: float = 25.0) -> bool:
    """Wait until `needle` appears in `path`. Log-driven waits beat fixed sleeps."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.is_file() and needle in path.read_text(errors="replace"):
            return True
        time.sleep(0.2)
    return False


# ---------------------------------------------------------------------------
# JWT
# ---------------------------------------------------------------------------

def _b64(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def mint_token(scopes: str, subject: str = "ma-core", ttl_hours: int = 1) -> str:
    """Mint an HS256 JWT matching what Control-Center's own generator emits.

    Hand-rolled rather than shelling out to `generate-token`, so the tests do not
    depend on which release's helper binaries happen to be staged.
    """
    now = int(time.time())
    header = {"alg": "HS256", "typ": "JWT"}
    payload = {
        "sub": subject,
        "aud": "control-center",
        "iss": "control-center-auth",
        "iat": now,
        "exp": now + ttl_hours * 3600,
        "scopes": scopes.split(),
        "jti": f"it-{now}-{os.getpid()}",
    }
    signing_input = f"{_b64(json.dumps(header).encode())}.{_b64(json.dumps(payload).encode())}"
    signature = hmac.new(JWT_SECRET.encode(), signing_input.encode(), hashlib.sha256).digest()
    return f"{signing_input}.{_b64(signature)}"


# ---------------------------------------------------------------------------
# TLS material
# ---------------------------------------------------------------------------

def write_tls_material(out_dir: Path) -> tuple[Path, Path, Path]:
    """Generate a CA plus a loopback server certificate. Returns (ca, cert, key)."""
    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import rsa
    from cryptography.x509.oid import NameOID
    import datetime
    import ipaddress

    out_dir.mkdir(parents=True, exist_ok=True)
    now = datetime.datetime.now(datetime.timezone.utc)

    ca_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "ma-integration-ca")])
    ca_cert = (
        x509.CertificateBuilder()
        .subject_name(ca_name)
        .issuer_name(ca_name)
        .public_key(ca_key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=5))
        .not_valid_after(now + datetime.timedelta(days=1))
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        .sign(ca_key, hashes.SHA256())
    )

    srv_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    srv_cert = (
        x509.CertificateBuilder()
        .subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")]))
        .issuer_name(ca_name)
        .public_key(srv_key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=5))
        .not_valid_after(now + datetime.timedelta(days=1))
        .add_extension(
            x509.SubjectAlternativeName([
                x509.DNSName("localhost"),
                x509.IPAddress(ipaddress.ip_address("127.0.0.1")),
            ]),
            critical=False,
        )
        .sign(ca_key, hashes.SHA256())
    )

    ca_path = out_dir / "ca.crt"
    cert_path = out_dir / "server.crt"
    key_path = out_dir / "server.key"
    ca_path.write_bytes(ca_cert.public_bytes(serialization.Encoding.PEM))
    cert_path.write_bytes(srv_cert.public_bytes(serialization.Encoding.PEM))
    key_path.write_bytes(
        srv_key.private_bytes(
            encoding=serialization.Encoding.PEM,
            format=serialization.PrivateFormat.TraditionalOpenSSL,
            encryption_algorithm=serialization.NoEncryption(),
        )
    )
    key_path.chmod(0o600)
    return ca_path, cert_path, key_path


# ---------------------------------------------------------------------------
# Processes
# ---------------------------------------------------------------------------

@dataclass
class CcServer:
    port: int
    log: Path
    tls: bool


@contextmanager
def cc_server(binary: Path, workdir: Path, tls_material: Optional[tuple[Path, Path, Path]]) -> Generator[CcServer]:
    """Run one Control-Center server. TLS is enabled when material is supplied."""
    port = free_port()
    log = workdir / "cc-server.log"

    env = dict(os.environ)
    # 1.0.0 reads JWT_SECRET; 1.1.0+ reads CC_JWT_SECRET. Set both so one harness
    # drives every version.
    env["JWT_SECRET"] = JWT_SECRET
    env["CC_JWT_SECRET"] = JWT_SECRET
    if tls_material:
        _ca, cert, key = tls_material
        env["CC_TLS_CERT"] = str(cert)
        env["CC_TLS_KEY"] = str(key)
    else:
        env["CC_ALLOW_INSECURE"] = "true"

    # The Rust server takes no host/port arguments — the Python
    # `control-center server start` wrapper passes the bind address through
    # SERVER_ADDR, so the harness does the same. HOME is redirected so the
    # generated server-identity file lands in the test directory rather than the
    # developer's config.
    env["SERVER_ADDR"] = f"127.0.0.1:{port}"
    env["HOME"] = str(workdir)
    env["XDG_CONFIG_HOME"] = str(workdir / ".config")
    (workdir / ".config" / "control-center").mkdir(parents=True, exist_ok=True)

    with log.open("wb") as fh:
        proc = subprocess.Popen(
            [str(binary)], stdout=fh, stderr=fh, env=env, start_new_session=True,
        )
    try:
        if not wait_port(port):
            raise RuntimeError(
                f"Control-Center did not open {port}:\n{log.read_text(errors='replace')[-2000:]}"
            )
        yield CcServer(port=port, log=log, tls=bool(tls_material))
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


@dataclass
class CcAgent:
    log: Path


@contextmanager
def cc_agent(
    binary: Path,
    workdir: Path,
    cc_port: int,
    token: Optional[str] = None,
    tls_ca: Optional[Path] = None,
) -> Generator[CcAgent]:
    """Attach a real Control-Center agent to a running server.

    The agent is needed because `agent_version` only exists on the wire once one
    is connected: with no agent the server stamps an empty string on its
    heartbeats, which is exactly why a server-only test cannot exercise the
    agent gate.

    Nothing here actuates. The agent registers and then idles, and the server
    emits a heartbeat carrying its version every five seconds — which is all the
    gate reads. That is what makes this runnable headless: `control-center-agent`
    reads `DISPLAY` only when it executes a command, and its startup probe for
    `xdotool` is advisory — a missing one never stops it registering. (The probe
    is `which xdotool` tested with `.output().is_ok()`, which is true whenever
    `which` *ran*, so it does not even warn. Control-Center's business, noted
    here only so this comment is not read as a promise that it would.)

    Configuration is entirely by environment — the agent takes no arguments.
    """
    log = workdir / "cc-agent.log"

    env = dict(os.environ)
    env["AGENT_SERVER_HOST"] = "127.0.0.1"
    env["AGENT_SERVER_PORT"] = str(cc_port)
    env["HOME"] = str(workdir)
    env["XDG_CONFIG_HOME"] = str(workdir / ".config")

    # The agent logs through `tracing_subscriber::fmt::init()`, which filters to
    # ERROR when RUST_LOG is unset — so a healthy agent writes nothing at all and
    # the readiness line never appears. Without this the harness cannot tell
    # "connected and idle" from "never started".
    env["RUST_LOG"] = env.get("RUST_LOG") or "info"

    # 1.1.0+ refuses to dial a plaintext server unless told to, and unlike
    # ma-core it has no auto-downgrade: it exits with
    # "TLS required: set AGENT_TLS_CA ... or AGENT_ALLOW_INSECURE".
    if tls_ca:
        env["AGENT_TLS_CA"] = str(tls_ca)
        env.pop("AGENT_ALLOW_INSECURE", None)
    else:
        env["AGENT_ALLOW_INSECURE"] = "true"
        env.pop("AGENT_TLS_CA", None)

    if token:
        env["CONTROL_CENTER_TOKEN"] = token
    else:
        env.pop("CONTROL_CENTER_TOKEN", None)

    with log.open("wb") as fh:
        proc = subprocess.Popen(
            [str(binary)], stdout=fh, stderr=fh, env=env, start_new_session=True,
        )
    try:
        # Registration, not the port: the agent dials out, so there is nothing to
        # poll for. Without this the test races the agent and reads a heartbeat
        # stamped with an empty version, which the gate correctly ignores — and
        # the test would then pass for the wrong reason.
        if not wait_for_file_line(log, "Ready to accept commands", timeout=30.0):
            raise RuntimeError(
                f"Control-Center agent did not register:\n"
                f"{log.read_text(errors='replace')[-2000:]}"
            )
        yield CcAgent(log=log)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


@dataclass
class MaCore:
    config_path: Path
    storage: Path
    log: Path
    socket_path: Path


@contextmanager
def ma_core(
    workdir: Path,
    cc_port: int,
    *,
    token: str = "",
    tls_ca: str = "",
    security: str = "auto",
    scheme: str = "http",
    max_version: str = "",
    allow_unsupported: bool = False,
    silence_timeout: int = 5,
) -> Generator[MaCore]:
    """Run ma-core against an isolated config, storage tree and Redis DB."""
    binary = ma_core_binary()
    if binary is None:
        raise RuntimeError("ma-core binary not built — run: cargo build -p ma-core")

    storage = workdir / "sessions"
    storage.mkdir(parents=True, exist_ok=True)

    # The IPC socket path must stay under SUN_LEN (~108 bytes) and ma-core chmods
    # its parent to 0700, so a short directory we own is required.
    sock_dir = Path(f"/tmp/ma-it-{os.getpid()}")
    if sock_dir.exists():
        shutil.rmtree(sock_dir, ignore_errors=True)
    sock_dir.mkdir(parents=True, exist_ok=True)
    sock_dir.chmod(0o700)
    socket_path = sock_dir / "ma.sock"

    config_path = workdir / "config.json"
    config_path.write_text(json.dumps({
        "redis_url": f"redis://127.0.0.1:6379/{REDIS_TEST_DB}",
        "ipc_socket_path": str(socket_path),
        "storage_path": str(storage),
        "storage_mode": "local",
        "control_center_addr": f"{scheme}://127.0.0.1:{cc_port}",
        "control_center_token": token,
        "control_center_tls_ca": tls_ca,
        "control_center_security": security,
        "control_center_max_version": max_version,
        "control_center_allow_unsupported": allow_unsupported,
        "the_eyes_addr": "",
        # 5s keeps the connect-only tests quick, but it is also exactly the
        # server's WatchCommands heartbeat interval — so anything that needs to
        # *receive* a heartbeat races it and usually loses. Those tests raise it.
        "silence_timeout_seconds": silence_timeout,
    }, indent=2))
    config_path.chmod(0o600)

    log = workdir / "ma-core.log"
    env = dict(os.environ)
    env["MEMORY_ARCHIVE_CONFIG"] = str(config_path)

    with log.open("wb") as fh:
        proc = subprocess.Popen(
            [str(binary)], stdout=fh, stderr=fh, env=env, start_new_session=True
        )
    try:
        if not wait_for_file_line(log, "IPC Unix socket server ready"):
            raise RuntimeError(
                f"ma-core did not start:\n{log.read_text(errors='replace')[-2000:]}"
            )
        yield MaCore(config_path=config_path, storage=storage, log=log, socket_path=socket_path)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
        shutil.rmtree(sock_dir, ignore_errors=True)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def cli(config_path: Path, *args: str, timeout: int = 90) -> subprocess.CompletedProcess:
    """Invoke the real `memory-archive` CLI against an isolated config."""
    env = dict(os.environ)
    env["MEMORY_ARCHIVE_CONFIG"] = str(config_path)
    return subprocess.run(
        ["memory-archive", *args],
        capture_output=True, text=True, env=env, timeout=timeout,
    )


@contextmanager
def cli_detached(config_path: Path, *args: str) -> Generator[subprocess.Popen]:
    """Run a `memory-archive` command without waiting for it to finish.

    `start` blocks until the session ends, which is correct: a healthy capture
    runs until an operator stops it. Any test that watches a session *while it is
    working* therefore cannot use `cli()` — it would block until the harness
    timeout and report a failure that is really the feature behaving properly.
    """
    env = dict(os.environ)
    env["MEMORY_ARCHIVE_CONFIG"] = str(config_path)
    proc = subprocess.Popen(
        ["memory-archive", *args],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        env=env, start_new_session=True,
    )
    try:
        yield proc
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


def register_session(config_path: Path, memory_name: str) -> str:
    result = cli(
        config_path, "session", "register",
        "--mode", "manual",
        "--os-type", "MACOS", "--os-version", "integration", "--os-arch", "x86_64",
        "--os-env-id", "integration",
        "--capture-server", "the-eyes-test", "--actuation-server", "control-center-test",
        "--memory-name", memory_name,
    )
    for line in result.stdout.splitlines():
        if "session_id" in line:
            return line.split(":", 1)[1].strip()
    raise RuntimeError(f"registration failed:\n{result.stdout}\n{result.stderr}")


def session_status(config_path: Path, session_id: str) -> str:
    result = cli(config_path, "status", "--session", session_id)
    for line in result.stdout.splitlines():
        if line.strip().startswith("status"):
            return line.split(":", 1)[1].strip()
    return ""


def reset_registry() -> None:
    """Flush the test Redis DB. Refuses to touch anything but DB 15."""
    assert REDIS_TEST_DB != 0, "refusing to flush the live registry"
    subprocess.run(
        ["redis-cli", "-n", str(REDIS_TEST_DB), "flushdb"],
        capture_output=True, check=False,
    )
