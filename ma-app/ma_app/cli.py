# /Memory-Archive/ma-app/ma_app/cli.py

import os
from typing import Optional
import typer
from pathlib import Path
from rich.console import Console

try:
    from ma_app import __version__
except ImportError:
    __version__ = "0.11.0"

app = typer.Typer(
    name="memory-archive",
    help="Memory Archive — structured memory creation for CUA training.",
    no_args_is_help=True,
)

console = Console()

@app.callback(invoke_without_command=True)
def _main(
    version: bool = typer.Option(
        False,
        "--version", "-v", "--v", "-version",
        is_eager=True,
        help="Show version and exit.",
    ),
) -> None:
    if version:
        console.print(f"[bold]memory-archive[/bold] {__version__}")
        raise typer.Exit()

# Session subcommand group
session_app = typer.Typer(help="Manage Memory Archive sessions.")
app.add_typer(session_app, name="session")

# TLS subcommand group
tls_app = typer.Typer(help="TLS certificate management for remote IPC.")
app.add_typer(tls_app, name="tls")

annotator_app = typer.Typer(help="Annotator queue and session claiming.")
app.add_typer(annotator_app, name="annotator")

@annotator_app.command("queue")
def annotator_queue() -> None:
    """List sessions available for human annotation."""
    from ma_app.ipc.client import IPCClient, IPCError

    try:
        with IPCClient() as client:
            response = client.send({"type": "list_annotation_queue"})
    except IPCError as e:
        console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)

    if response.get("type") == "error":
        console.print(f"[red]{response.get('message')}[/red]")
        raise typer.Exit(code=1)

    sessions = response.get("sessions", [])
    if not sessions:
        console.print("[yellow]No sessions available for annotation.[/yellow]")
        return

    console.print(f"\n[bold]Annotation Queue — {len(sessions)} session(s) available:[/bold]\n")
    for s in sessions:
        console.print(
            f"  [cyan]{s['session_id']}[/cyan]  "
            f"{s['memory_name']}  "
            f"[dim]{s['total_steps']} steps  {s['created_at'][:10]}[/dim]"
        )
    console.print(f"\nClaim with: [bold]memory-archive annotator claim[/bold]  (auto)")
    console.print(f"       or:  [bold]memory-archive annotator claim --session <id>[/bold]")


@annotator_app.command("claim")
def annotator_claim(
    session: str = typer.Option("", "--session", "-s", help="Session ID to claim (empty = auto-claim oldest)"),
) -> None:
    """Claim a session from the annotation queue and open the TUI."""
    from ma_app.ipc.client import IPCClient, IPCError
    from ma_app.config.settings import Settings
    from ma_app.storage.sync_worker import init_worker, shutdown_worker

    settings = Settings.load()
    if not settings.annotator_id:
        console.print(
            "[red]annotator_id is not configured.[/red]\n"
            "Run: memory-archive config --annotator-id <your-name>"
        )
        raise typer.Exit(code=1)

    try:
        with IPCClient() as client:
            response = client.send({
                "type": "claim_session",
                "session_id": session.strip(),
            })
    except IPCError as e:
        console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)

    if response.get("type") == "error":
        code = response.get("code", "")
        if code == "QUEUE_EMPTY":
            console.print("[yellow]No sessions available — the queue is empty.[/yellow]")
        elif code == "CLAIM_CONFLICT":
            console.print(f"[yellow]Session already claimed by another annotator. Try a different one.[/yellow]")
        else:
            console.print(f"[red]{response.get('message')}[/red]")
        raise typer.Exit(code=1)

    if response.get("type") == "claim_conflict":
        console.print("[yellow]That session was just claimed by another annotator. Try again or choose a different session.[/yellow]")
        raise typer.Exit(code=1)

    claimed_session_id = response["session_id"]
    claim_id = response["claim_id"]

    console.print(f"[green]Claimed session: {claimed_session_id}[/green]")
    console.print("[dim]Heartbeat active — claim refreshes every 5 minutes while TUI is open.[/dim]")

    worker_active = settings.storage_mode == "local"
    if worker_active:
        init_worker(settings)

    try:
        from ma_app.tui.session_loader import SessionLoader, LoadError
        try:
            session_data = SessionLoader(claimed_session_id, claim_id=claim_id).load()
        except LoadError as e:
            console.print(f"[red]Failed to load session: {e}[/red]")
            with IPCClient() as client:
                client.send({
                    "type": "release_session",
                    "session_id": claimed_session_id,
                    "claim_id": claim_id,
                })
            raise typer.Exit(code=1)

        from ma_app.tui.app import AnnotationApp
        result = AnnotationApp(session_data).run()
        if result == "compile":
            console.print("\n[green]Launching compiler...[/green]")
            from ma_app.compiler.scaffolder import run_compile
            from ma_app.tui.app import CompilerApp
            from ma_app.compiler.finalizer import finalize_memory
            memory_md_path = run_compile(claimed_session_id, console)
            if memory_md_path:
                compiler_result = CompilerApp(claimed_session_id, memory_md_path).run()
                if compiler_result == "complete":
                    finalize_memory(claimed_session_id, memory_md_path, console)
    finally:
        if worker_active:
            shutdown_worker()

annotator_admin_app = typer.Typer(help="Annotator management — admin operations (requires admin IPC connection).")
app.add_typer(annotator_admin_app, name="annotator-admin")


@annotator_admin_app.command("register")
def annotator_admin_register(
    annotator_id: str = typer.Option(..., "--annotator-id", help="Unique identifier for the new annotator"),
    allowed_tenants: str = typer.Option("", "--allowed-tenants", help="Comma-separated tenant ID prefixes this annotator can see. Empty = all tenants."),
    max_claims: int = typer.Option(0, "--max-claims", help="Maximum concurrent session claims. 0 = unlimited."),
) -> None:
    """Register a new annotator and receive their unique key (shown once only)."""
    from ma_app.ipc.client import IPCClient, IPCError

    tenant_list = [t.strip() for t in allowed_tenants.split(",") if t.strip()] if allowed_tenants.strip() else []

    try:
        with IPCClient() as client:
            response = client.send({
                "type": "register_annotator",
                "annotator_id": annotator_id,
                "allowed_tenant_ids": tenant_list,
                "max_concurrent_claims": max_claims,
            })
    except IPCError as e:
        console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)

    if response.get("type") != "annotator_registered":
        console.print(f"[red]Unexpected response: {response}[/red]")
        raise typer.Exit(code=1)

    plaintext_key = response.get("plaintext_key", "")
    console.print(f"\n[green]Annotator registered: {annotator_id}[/green]")
    console.print(f"\n  [bold yellow]Plaintext key (shown once — store and distribute securely):[/bold yellow]")
    console.print(f"  [cyan]{plaintext_key}[/cyan]")
    if tenant_list:
        console.print(f"\n  Allowed tenants : {', '.join(tenant_list)}")
    else:
        console.print(f"\n  Allowed tenants : (all — no restrictions)")
    console.print(f"  Max claims      : {max_claims if max_claims > 0 else 'unlimited'}")
    console.print(
        f"\n  Generate their profile next:\n"
        f"    [bold]memory-archive annotator-admin generate-profile --annotator-id {annotator_id}[/bold]"
    )


@annotator_admin_app.command("deactivate")
def annotator_admin_deactivate(
    annotator_id: str = typer.Option(..., "--annotator-id", help="Annotator to deactivate"),
) -> None:
    """Deactivate an annotator. Their key is rejected on next auth attempt."""
    from ma_app.ipc.client import IPCClient, IPCError

    try:
        with IPCClient() as client:
            response = client.send({"type": "deactivate_annotator", "annotator_id": annotator_id})
    except IPCError as e:
        console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)

    if response.get("type") != "annotator_deactivated":
        console.print(f"[red]Unexpected response: {response}[/red]")
        raise typer.Exit(code=1)

    console.print(f"[green]Annotator '{annotator_id}' deactivated.[/green]")


@annotator_admin_app.command("rotate-key")
def annotator_admin_rotate_key(
    annotator_id: str = typer.Option(..., "--annotator-id", help="Annotator whose key to rotate"),
) -> None:
    """Generate a new key for an annotator. Old key is immediately invalid."""
    from ma_app.ipc.client import IPCClient, IPCError

    try:
        with IPCClient() as client:
            response = client.send({"type": "rotate_annotator_key", "annotator_id": annotator_id})
    except IPCError as e:
        console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)

    if response.get("type") != "annotator_key_rotated":
        console.print(f"[red]Unexpected response: {response}[/red]")
        raise typer.Exit(code=1)

    new_key = response.get("new_plaintext_key", "")
    console.print(f"\n[green]Key rotated for annotator '{annotator_id}'.[/green]")
    console.print(f"\n  [bold yellow]New plaintext key (shown once — store and distribute securely):[/bold yellow]")
    console.print(f"  [cyan]{new_key}[/cyan]")
    console.print(
        f"\n  Generate an updated profile:\n"
        f"    [bold]memory-archive annotator-admin generate-profile --annotator-id {annotator_id}[/bold]"
    )


@annotator_admin_app.command("list")
def annotator_admin_list() -> None:
    """List all registered annotators with live claim counts."""
    from ma_app.ipc.client import IPCClient, IPCError

    try:
        with IPCClient() as client:
            response = client.send({"type": "list_annotators"})
    except IPCError as e:
        console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)

    if response.get("type") != "annotator_list":
        console.print(f"[red]Unexpected response: {response}[/red]")
        raise typer.Exit(code=1)

    annotators = response.get("annotators", [])
    if not annotators:
        console.print("[yellow]No annotators registered.[/yellow]")
        return

    console.print(f"\n[bold]Registered annotators ({len(annotators)}):[/bold]\n")
    for a in annotators:
        status_color = "green" if a.get("status") == "active" else "red"
        claims = a.get("current_claims", 0)
        max_c = a.get("max_concurrent_claims", 0)
        max_label = str(max_c) if max_c > 0 else "unlimited"
        tenants = a.get("allowed_tenant_ids", [])
        tenant_label = ", ".join(tenants) if tenants else "(all)"
        console.print(
            f"  [{status_color}]{a['annotator_id']}[/{status_color}]  "
            f"status={a.get('status', '?')}  "
            f"claims={claims}/{max_label}  "
            f"tenants={tenant_label}  "
            f"last_auth={a.get('last_auth_at', 'never') or 'never'}"
        )


@annotator_admin_app.command("generate-profile")
def annotator_admin_generate_profile(
    annotator_id: str = typer.Option(..., "--annotator-id", help="Annotator to generate a profile for"),
) -> None:
    """Generate a connection profile for an annotator.

    The profile encodes the server address, TLS fingerprint, annotator_id, and
    annotator key into a single base64 string. The annotator installs it with:
      memory-archive annotator setup <profile-string>

    You must know the annotator's current plaintext key to generate the profile.
    If the key is unknown, rotate it first with:
      memory-archive annotator-admin rotate-key --annotator-id <id>
    """
    import base64
    import json as _json
    from ma_app.config.settings import Settings
    from ma_app.ipc.client import IPCClient, IPCError

    settings = Settings.load()

    missing = []
    if not settings.ma_core_addr:
        missing.append("ma_core_addr (run: memory-archive config --ma-core-addr <host:port>)")
    if not settings.ipc_server_fingerprint:
        missing.append("ipc_server_fingerprint (run: memory-archive tls fingerprint, then configure it)")
    if missing:
        console.print("[red]Cannot generate profile — missing configuration:[/red]")
        for item in missing:
            console.print(f"  [yellow]• {item}[/yellow]")
        raise typer.Exit(code=1)

    # Verify the annotator exists and is active via IPC.
    try:
        with IPCClient() as client:
            response = client.send({"type": "list_annotators"})
    except IPCError as e:
        console.print(f"[red]Failed to reach ma-core: {e}[/red]")
        raise typer.Exit(code=1)

    annotators = response.get("annotators", [])
    target = next((a for a in annotators if a.get("annotator_id") == annotator_id), None)
    if target is None:
        console.print(f"[red]Annotator '{annotator_id}' not found. Register them first:[/red]")
        console.print(f"  memory-archive annotator-admin register --annotator-id {annotator_id}")
        raise typer.Exit(code=1)

    if target.get("status") != "active":
        console.print(f"[red]Annotator '{annotator_id}' is not active (status: {target.get('status', '?')}).[/red]")
        raise typer.Exit(code=1)

    console.print(
        f"\n[yellow]Enter the plaintext key for '{annotator_id}'.[/yellow]\n"
        f"  If the key is unknown, rotate it first:\n"
        f"    memory-archive annotator-admin rotate-key --annotator-id {annotator_id}\n"
    )
    annotator_key = typer.prompt("Annotator key (will not be echoed)", hide_input=True)

    if not annotator_key.strip():
        console.print("[red]Key cannot be empty.[/red]")
        raise typer.Exit(code=1)

    profile_data = _json.dumps({
        "ma_core_addr": settings.ma_core_addr,
        "ipc_server_fingerprint": settings.ipc_server_fingerprint,
        "annotator_id": annotator_id,
        "annotator_key": annotator_key.strip(),
    })
    profile_string = base64.b64encode(profile_data.encode()).decode()

    console.print(f"\n[bold]Annotator Connection Profile — {annotator_id}:[/bold]")
    console.print(f"\n  [cyan]{profile_string}[/cyan]")
    console.print(
        f"\n  The annotator runs on their machine:\n"
        f"    memory-archive annotator setup <profile-string>"
    )


@annotator_app.command("setup")
def annotator_setup(
    profile: str = typer.Argument(..., help="Connection profile string from your operator"),
) -> None:
    """Configure this machine as an annotator using a profile from your operator.

    The profile contains your identity and key — no --annotator-id flag is needed.

    Run this once. After setup, use:
      memory-archive annotator queue
      memory-archive annotator claim
    """
    import base64
    import json as _json

    try:
        decoded = base64.b64decode(profile.strip()).decode()
        data = _json.loads(decoded)
    except Exception:
        console.print(
            "[red]Invalid profile string. Make sure you copied it completely — it should be "
            "a long base64 string with no spaces or line breaks.[/red]"
        )
        raise typer.Exit(code=1)

    # Support both the new format (with annotator_id) and the old format (without).
    required = ["ma_core_addr", "ipc_server_fingerprint", "annotator_key"]
    missing = [k for k in required if not data.get(k)]
    if missing:
        console.print(f"[red]Profile is missing fields: {', '.join(missing)}. Ask your operator to regenerate it.[/red]")
        raise typer.Exit(code=1)

    # New profiles carry annotator_id inside; old profiles required --annotator-id.
    profile_annotator_id = data.get("annotator_id", "").strip()

    from ma_app.config.settings import Settings
    settings = Settings.load()
    settings.ma_core_addr = data["ma_core_addr"]
    settings.ipc_server_fingerprint = data["ipc_server_fingerprint"]
    settings.annotator_key = data["annotator_key"]
    if profile_annotator_id:
        settings.annotator_id = profile_annotator_id
    settings.save()

    identity = settings.annotator_id
    console.print(f"\n[green]Annotator setup complete.[/green]")
    console.print(f"  Server  : {data['ma_core_addr']}")
    console.print(f"  Identity: {identity}")
    console.print(f"\nYou're ready. Run:")
    console.print(f"  [bold]memory-archive annotator queue[/bold]   — see available sessions")
    console.print(f"  [bold]memory-archive annotator claim[/bold]   — claim and annotate one")

@session_app.command("register")
def session_register(
    mode: str = typer.Option(..., help="Session mode: 'manual' or 'automated'"),
    os_type: str = typer.Option(..., "--os-type", help="OS type: LINUX | WINDOWS | MACOS"),
    os_version: str = typer.Option(..., "--os-version", help="OS version string e.g. 'Ubuntu 24.04 LTS'"),
    os_arch: str = typer.Option(..., "--os-arch", help="Architecture e.g. x86_64"),
    os_env_id: str = typer.Option(..., "--os-env-id", help="OS environment ID from the orchestration layer"),
    capture_server: str = typer.Option(..., "--capture-server", help="The-Eyes server ID"),
    actuation_server: str = typer.Option(..., "--actuation-server", help="Control-Center server ID"),
    memory_name: str = typer.Option(..., "--memory-name", help="Name for this memory (used as directory name)"),
    tenant_id: str = typer.Option("", "--tenant-id", help="Tenant ID (overrides config value if set)"),
) -> None:
    """Register a new session in the Memory Archive registry."""
    from ma_app.session.register import register_session
    from ma_app.ipc.client import IPCError

    try:
        from ma_app.config.settings import Settings
        settings = Settings.load()
        resolved_tenant = tenant_id.strip() or settings.tenant_id.strip() or None
        session_id = register_session(
            mode=mode,
            os_type=os_type,
            os_version=os_version,
            os_arch=os_arch,
            os_env_id=os_env_id,
            capture_server=capture_server,
            actuation_server=actuation_server,
            memory_name=memory_name,
            tenant_id=resolved_tenant,
        )
        console.print(f"[green]Session registered.[/green]")
        console.print(f"  session_id  : [bold]{session_id}[/bold]")
        console.print(f"  memory_name : {memory_name}")
        console.print(f"  mode        : {mode}")
    except (IPCError, ValueError) as e:
        console.print(f"[red]Registration failed: {e}[/red]")
        raise typer.Exit(code=1)


# Top-level subcommands

@app.command()
def start(
    session: str = typer.Option(..., "--session", "-s", help="Session ID to start watching"),
) -> None:
    """Start watching a registered session. Blocks until session ends."""
    from ma_app.ipc.client import IPCClient, IPCError
    from ma_app.storage import FileWrittenEvent, SyncWorker
    from ma_app.config.settings import Settings
    from ma_app.storage.sync_worker import init_worker, shutdown_worker

    settings = Settings.load()
    sync_worker = init_worker(settings) if settings.storage_mode == "local" else None

    try:
        with IPCClient() as client:
            response = client.send({"type": "start_watch", "session_id": session})
            if response.get("type") != "watch_started":
                console.print(f"[red]Unexpected response: {response}[/red]")
                raise typer.Exit(code=1)

            memory_path = response.get("memory_path", "")
            if memory_path:
                import threading
                from ma_app.storage.sync_log import SyncLog
                from pathlib import Path as _Path

                if sync_worker is not None:
                    def _resume_pending():
                        log = SyncLog(_Path(memory_path), session)
                        pending = log.pending_files()
                        if pending:
                            console.print(
                                f"[yellow]Resuming sync for {len(pending)} unsynced "
                                f"file(s) from previous session...[/yellow]"
                            )
                            for rel_path in pending:
                                abs_path = str(_Path(memory_path) / rel_path)
                                sync_worker.enqueue(FileWrittenEvent(
                                    session_id=session,
                                    relative_path=rel_path,
                                    abs_path=abs_path,
                                ))

                    threading.Thread(target=_resume_pending, daemon=True).start()

            console.print(f"[green]Watching session: {session}[/green]")
            console.print("Waiting for session to end... (Ctrl+C to detach)")

            while True:
                push = client.recv()
                msg_type = push.get("type")

                if msg_type == "file_written":
                    if sync_worker is not None:
                        sync_worker.enqueue(FileWrittenEvent(
                            session_id=push.get("session_id", ""),
                            relative_path=push.get("relative_path", ""),
                            abs_path=push.get("abs_path", ""),
                        ))

                elif msg_type == "session_complete":
                    total_steps = push.get("total_steps", 0)
                    console.print(f"\n[green]Session complete.[/green]")
                    console.print(f"  total_steps : {total_steps}")
                    if sync_worker is not None:
                        alerts = sync_worker.drain_alerts()
                        if alerts:
                            console.print("\n[yellow]Sync warnings:[/yellow]")
                            for alert in alerts:
                                console.print(f"  [yellow]{alert}[/yellow]")
                    console.print(f"\nRun next: [bold]memory-archive annotate --session {session}[/bold]")
                    break

                elif msg_type == "session_disconnected":
                    reason = push.get("reason", "unknown")
                    console.print(f"\n[yellow]Session disconnected: {reason}[/yellow]")
                    break

                else:
                    console.print(f"\n[yellow]Received unexpected push: {push}[/yellow]")

    except KeyboardInterrupt:
        console.print("\n[yellow]Detached — session continues in background.[/yellow]")
    except IPCError as e:
        console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)
    finally:
        if sync_worker is not None:
            shutdown_worker()

@app.command()
def automated(
    session: str = typer.Option(..., "--session", "-s", help="Session ID to watch in automated mode"),
) -> None:
    """Run automated reasoning for a session — calls VLM per step and sends results to ma-core."""
    from ma_app.config.settings import Settings
    from ma_app.ipc.client import IPCClient, IPCError
    from ma_app.model import get_router, ReasoningPipeline

    settings = Settings.load()

    if settings.storage_mode != "cloud_primary":
        console.print(
            "[red]Automated mode requires storage_mode = cloud_primary.[/red]\n"
            "Run: memory-archive config --storage-mode cloud_primary"
        )
        raise typer.Exit(code=1)

    try:
        router = get_router(settings)
    except ValueError as e:
        console.print(f"[red]Model router configuration error: {e}[/red]")
        raise typer.Exit(code=1)

    pipeline = ReasoningPipeline(router=router, settings=settings)

    try:
        with IPCClient() as client:
            response = client.send({"type": "start_watch", "session_id": session})
            if response.get("type") != "watch_started":
                console.print(f"[red]Unexpected response: {response}[/red]")
                raise typer.Exit(code=1)

            # Build per-session router when the session has per-session VLM config.
            # set_session_config is a no-op if model_provider is absent or empty.
            pipeline.set_session_config(
                session_id=session,
                primary_provider=response.get("model_provider", ""),
                primary_endpoint=response.get("model_endpoint", ""),
                primary_key_ref=response.get("model_api_key_ref", ""),
                fallback_provider=response.get("fallback_model_provider", ""),
                fallback_endpoint=response.get("fallback_model_endpoint", ""),
                fallback_key_ref=response.get("fallback_api_key_ref", ""),
            )

            console.print(f"[green]Automated reasoning active: {session}[/green]")
            console.print("Waiting for steps... (Ctrl+C to stop)")

            while True:
                push = client.recv()
                msg_type = push.get("type")

                if msg_type == "step_ready_for_reasoning":
                    pipeline.submit(push)

                elif msg_type == "session_complete":
                    total_steps = push.get("total_steps", 0)
                    console.print(f"\n[green]Session complete.[/green]")
                    console.print(f"  total_steps : {total_steps}")
                    console.print(f"\nRun next: [bold]memory-archive annotate --session {session}[/bold]")
                    break

                elif msg_type == "session_disconnected":
                    reason = push.get("reason", "unknown")
                    console.print(f"\n[yellow]Session disconnected: {reason}[/yellow]")
                    break

                elif msg_type == "reasoning_degraded_event":
                    pipeline.mark_session_degraded(push.get("session_id", session))
                    console.print(
                        f"\n[yellow]Reasoning degraded: {session} — "
                        "circuit breaker opened, session will fall back to human annotation.[/yellow]"
                    )

    except KeyboardInterrupt:
        console.print("\n[yellow]Stopping automated reasoning daemon...[/yellow]")
    except IPCError as e:
        console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)
    finally:
        pipeline.shutdown(wait=True, timeout=30.0)


@app.command()
def done(
    session: str = typer.Option(..., "--session", "-s", help="Session ID to stop watching"),
) -> None:
    """Signal that actuation is complete and finalise capture files."""
    from ma_app.ipc.client import IPCClient, IPCError

    try:
        with IPCClient() as client:
            response = client.send({"type": "done", "session_id": session})
            if response.get("type") != "session_complete":
                console.print(f"[red]Unexpected response: {response}[/red]")
                raise typer.Exit(code=1)

            total_steps = response.get("total_steps", 0)
            console.print(f"[green]Session complete.[/green]")
            console.print(f"  session_id  : {session}")
            console.print(f"  total_steps : {total_steps}")
            console.print("\nRun next: [bold]memory-archive annotate --session {session}[/bold]")
    except IPCError as e:
        console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)

@app.command()
def annotate(
    session: str = typer.Option(..., "--session", "-s", help="Session ID to annotate"),
) -> None:
    """Open the TUI annotation interface for a completed session."""
    from ma_app.tui.session_loader import SessionLoader, LoadError
    from ma_app.config.settings import Settings
    from ma_app.storage.sync_worker import init_worker, shutdown_worker

    settings = Settings.load()
    worker_active = settings.storage_mode == "local"
    if worker_active:
        init_worker(settings)

    try:
        try:
            session_data = SessionLoader(session).load()
        except LoadError as e:
            console.print(f"[red]Failed to load session: {e}[/red]")
            raise typer.Exit(code=1)

        if session_data.is_resume:
            console.print(
                f"[yellow]Resuming — {session_data.annotated_steps} steps already annotated, "
                f"starting at step {session_data.cursor_step + 1}[/yellow]"
            )
        else:
            console.print(
                f"[green]Starting annotation — {session_data.total_steps} steps to annotate[/green]"
            )

        from ma_app.tui.app import AnnotationApp
        result = AnnotationApp(session_data).run()
        if result == "compile":
            console.print("\n[green]Launching compiler...[/green]")
            from ma_app.compiler.scaffolder import run_compile
            from ma_app.tui.app import CompilerApp
            from ma_app.compiler.finalizer import finalize_memory
            memory_md_path = run_compile(session, console)
            if memory_md_path:
                compiler_result = CompilerApp(session, memory_md_path).run()
                if compiler_result == "complete":
                    finalize_memory(session, memory_md_path, console)
    finally:
        if worker_active:
            shutdown_worker()

@app.command()
def compile(
    session: str = typer.Option(..., "--session", "-s", help="Session ID to compile memory.md for"),
) -> None:
    """Scaffold and edit memory.md from a fully annotated session."""
    from ma_app.compiler.scaffolder import run_compile
    from ma_app.tui.app import CompilerApp
    from ma_app.compiler.finalizer import finalize_memory
    from ma_app.config.settings import Settings
    from ma_app.storage.sync_worker import init_worker, shutdown_worker

    settings = Settings.load()
    worker_active = settings.storage_mode == "local"
    if worker_active:
        init_worker(settings)

    try:
        memory_md_path = run_compile(session, console)
        if memory_md_path:
            compiler_result = CompilerApp(session, memory_md_path).run()
            if compiler_result == "complete":
                finalize_memory(session, memory_md_path, console)
    finally:
        if worker_active:
            shutdown_worker()

# Status subcommand
@app.command()
def status(
    session: str = typer.Option(..., "--session", "-s", help="Session ID to inspect"),
) -> None:
    """Show current status of a session from the registry."""
    from ma_app.session.register import get_session_status
    from ma_app.ipc.client import IPCError

    try:
        data = get_session_status(session)
        console.print(f"\n[bold]Session: {session}[/bold]")
        for key, value in sorted(data.items()):
            console.print(f"  {key:<25}: {value}")
    except IPCError as e:
        console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)

# Config subcommand
@app.command()
def config(
    storage_path: str = typer.Option(None, "--storage-path", help="Local path for memory storage"),
    storage_mode: str = typer.Option(None, "--storage-mode", help="Storage mode: local | cloud_primary"),
    control_center_addr: str = typer.Option(None, "--control-center-addr", help="Control-Center gRPC address e.g. http://127.0.0.1:50051"),
    the_eyes_addr: str = typer.Option(None, "--the-eyes-addr", help="The-Eyes HTTP server address e.g. http://127.0.0.1:8080"),
    the_eyes_poll_interval: int = typer.Option(None, "--the-eyes-poll-interval", help="The-Eyes liveness poll interval in seconds (default: 10)"),
    cloud: str = typer.Option(None, "--cloud", help="Cloud provider: aws | azure | gcp"),
    aws_bucket: str = typer.Option(None, "--aws-bucket", help="AWS S3 bucket name"),
    aws_region: str = typer.Option(None, "--aws-region", help="AWS S3 region e.g. us-east-1"),
    azure_container: str = typer.Option(None, "--azure-container", help="Azure Storage container or file share name"),
    azure_account: str = typer.Option(None, "--azure-account", help="Azure Storage account name"),
    azure_storage_type: str = typer.Option(None, "--azure-storage-type", help="Azure storage type: auto | blob | adls | files"),
    gcp_bucket: str = typer.Option(None, "--gcp-bucket", help="GCP Cloud Storage bucket name"),
    gcp_project: str = typer.Option(None, "--gcp-project", help="GCP project ID (optional)"),
    ma_core_addr: str = typer.Option(None, "--ma-core-addr", help="Remote ma-core address e.g. 192.168.1.10:9000"),
    ipc_port: int = typer.Option(None, "--ipc-port", help="TCP port for ma-core IPC listener (enables TCP mode)"),
    ipc_bind_addr: str = typer.Option(None, "--ipc-bind-addr", help="Bind address for ma-core TCP listener (default: 0.0.0.0)"),
    ipc_server_fingerprint: str = typer.Option(None, "--ipc-server-fingerprint", help="SHA-256 fingerprint of the ma-core server certificate for TLS verification (AA:BB:CC:...)"),
    annotator_id: str = typer.Option(None, "--annotator-id", help="Annotator identity (e.g. your name) — used to claim sessions"),
    annotator_key: str = typer.Option(None, "--annotator-key", help="Shared annotator key for queue access"),
    tenant_id: str = typer.Option(None, "--tenant-id", help="Tenant identifier for cost attribution and multi-tenant routing"),
    kafka_broker: str = typer.Option(None, "--kafka-broker", help="Kafka broker address e.g. localhost:9092"),
    redis_url: str = typer.Option(None, "--redis-url", help="Redis connection URL"),
    silence_timeout: int = typer.Option(None, "--silence-timeout", help="Tool silence timeout in seconds"),
    metadata_flush_interval: int = typer.Option(None, "--metadata-flush-interval", help="Metadata flush interval in steps (cloud_primary mode, default: 10)"),
    temp_session_dir: str = typer.Option(None, "--temp-session-dir", help="Temp directory for cloud_primary read-back (default: system temp dir / ma-sessions)"),
    show: bool = typer.Option(False, "--show", help="Print current configuration"),
) -> None:
    """Read or update Memory Archive configuration."""
    from ma_app.config.settings import Settings

    settings = Settings.load()
    changed = False

    if storage_mode:
        if storage_mode not in ("local", "cloud_primary"):
            console.print("[red]--storage-mode must be one of: local, cloud_primary[/red]")
            raise typer.Exit(code=1)
        settings.storage_mode = storage_mode
        changed = True
    if storage_path:
        p = Path(storage_path).expanduser().resolve()
        if not p.exists():
            try:
                p.mkdir(parents=True, exist_ok=True)
            except OSError as e:
                console.print(f"[red]Cannot create storage path: {e}[/red]")
                raise typer.Exit(code=1)
        if not p.is_dir():
            console.print(f"[red]Storage path is not a directory: {p}[/red]")
            raise typer.Exit(code=1)
        if not os.access(p, os.W_OK):
            console.print(f"[red]Storage path is not writable: {p}[/red]")
            raise typer.Exit(code=1)
        settings.storage_path = str(p)
        changed = True
    if control_center_addr:
        settings.control_center_addr = control_center_addr
        changed = True
    if the_eyes_addr:
        settings.the_eyes_addr = the_eyes_addr
        changed = True
    if the_eyes_poll_interval:
        settings.the_eyes_poll_interval_seconds = the_eyes_poll_interval
        changed = True
    if cloud:
        if cloud not in ("aws", "azure", "gcp"):
            console.print("[red]--cloud must be one of: aws, azure, gcp[/red]")
            raise typer.Exit(code=1)
        settings.cloud.provider = cloud
        changed = True
    if aws_bucket:
        settings.cloud.aws.bucket = aws_bucket
        changed = True
    if aws_region:
        settings.cloud.aws.region = aws_region
        changed = True
    if azure_container:
        settings.cloud.azure.container = azure_container
        changed = True
    if azure_account:
        settings.cloud.azure.account = azure_account
        changed = True
    if azure_storage_type:
        if azure_storage_type not in ("auto", "blob", "adls", "files"):
            console.print("[red]--azure-storage-type must be one of: auto, blob, adls, files[/red]")
            raise typer.Exit(code=1)
        settings.cloud.azure.storage_type = azure_storage_type
        changed = True
    if gcp_bucket:
        settings.cloud.gcp.bucket = gcp_bucket
        changed = True
    if gcp_project:
        settings.cloud.gcp.project = gcp_project
        changed = True
    if ma_core_addr is not None:
        settings.ma_core_addr = ma_core_addr
        changed = True
    if ipc_port is not None:
        settings.ipc_port = ipc_port
        changed = True
    if ipc_bind_addr is not None:
        settings.ipc_bind_addr = ipc_bind_addr
        changed = True
    if ipc_server_fingerprint is not None:
        settings.ipc_server_fingerprint = ipc_server_fingerprint
        changed = True
    if annotator_id is not None:
        settings.annotator_id = annotator_id
        changed = True
    if annotator_key is not None:
        settings.annotator_key = annotator_key
        changed = True
    if tenant_id is not None:
        settings.tenant_id = tenant_id
        changed = True
    if kafka_broker:
        settings.kafka_broker = kafka_broker
        changed = True
    if redis_url:
        settings.redis_url = redis_url
        changed = True
    if silence_timeout is not None:
        settings.silence_timeout_seconds = silence_timeout
        changed = True
    if metadata_flush_interval is not None:
        settings.metadata_flush_interval = int(metadata_flush_interval)
        changed = True
    if temp_session_dir is not None:
        settings.temp_session_dir = temp_session_dir
        changed = True

    if changed:
        settings.save()

    if show or not changed:
        console.print("\n[bold]Memory Archive configuration:[/bold]")
        console.print(settings.display())


@tls_app.command("fingerprint")
def tls_fingerprint() -> None:
    """Print the SHA-256 fingerprint of the ma-core server certificate.

    Run this on the ma-core server. Share the fingerprint string with annotators
    who configure it with:
      memory-archive config --ipc-server-fingerprint <fingerprint>

    No file distribution is needed — just the fingerprint string.
    """
    import base64
    import hashlib

    cert_path = Path.home() / ".memory-archive" / "ipc-cert.pem"

    if not cert_path.exists():
        console.print(
            "[red]Server certificate not found.[/red]\n"
            f"Expected at: {cert_path}\n"
            "Start ma-core with ipc_port configured to generate TLS assets automatically."
        )
        raise typer.Exit(code=1)

    try:
        pem_data = cert_path.read_text(encoding="ascii")
        b64 = "".join(
            line for line in pem_data.splitlines()
            if not line.startswith("-----")
        )
        der_bytes = base64.b64decode(b64)
        digest = hashlib.sha256(der_bytes).hexdigest().upper()
        formatted = ":".join(digest[i:i+2] for i in range(0, len(digest), 2))

        console.print(f"\n[bold]ma-core Server Certificate SHA-256 Fingerprint:[/bold]")
        console.print(f"  [cyan]{formatted}[/cyan]")
        console.print(f"\n  Certificate: {cert_path}")
        console.print(
            "\n  Use this fingerprint when generating the annotator connection profile:\n"
            "    memory-archive annotator profile"
        )
    except Exception as e:
        console.print(f"[red]Failed to compute fingerprint: {e}[/red]")
        raise typer.Exit(code=1)


@app.command()
def cost(
    session: str = typer.Option(..., "--session", "-s", help="Session ID to report cost for"),
    detailed: bool = typer.Option(False, "--detailed", help="Show per-step token breakdown (scans reasoning.jsonl)"),
) -> None:
    """Show token usage and estimated cost for a completed session.

    Rates are resolved per model_id via the signed pricing registry.
    Falls back to per-provider config rates, then reports 'unknown' if
    neither source has rates for the model.
    """
    from ma_app.ipc.client import IPCClient, IPCError
    from ma_app.config.settings import Settings
    from ma_app.storage.fetch import fetch_session_if_missing
    from ma_app.model.pricing import get_registry
    import json as _json

    settings = Settings.load()

    try:
        with IPCClient() as client:
            response = client.send({"type": "get_session_status", "session_id": session})
    except IPCError as e:
        console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)

    if response.get("type") == "error":
        console.print(f"[red]{response.get('message')}[/red]")
        raise typer.Exit(code=1)

    record = response.get("session", {})
    memory_path = record.get("memory_path", "")
    if not memory_path:
        console.print("[red]Session has no memory_path in registry.[/red]")
        raise typer.Exit(code=1)

    try:
        memory_dir = fetch_session_if_missing(memory_path, session, settings)
    except RuntimeError as e:
        console.print(f"[red]Failed to fetch session files: {e}[/red]")
        raise typer.Exit(code=1)

    # Load pricing registry (network fetch + verify at startup, cached 24h).
    registry = get_registry()

    # Read headline token counts and per-provider breakdown from metadata.json.
    # This avoids scanning the potentially large reasoning.jsonl for every invocation.
    meta_path = memory_dir / "metadata.json"
    metadata_totals: dict = {}
    if meta_path.exists():
        try:
            metadata_totals = _json.loads(meta_path.read_text(encoding="utf-8"))
        except (_json.JSONDecodeError, OSError):
            pass

    total_input: int = metadata_totals.get("total_input_tokens") or 0
    total_output: int = metadata_totals.get("total_output_tokens") or 0
    # token_costs_by_provider: {provider_name: {input_tokens, output_tokens}}
    meta_provider_counts: dict[str, dict[str, int]] = metadata_totals.get("token_costs_by_provider") or {}

    # Scan reasoning.jsonl for: model_id per provider (for registry lookup),
    # step count, and step-level data when --detailed is requested.
    jsonl_path = memory_dir / "reasoning" / "reasoning.jsonl"
    if not jsonl_path.exists():
        console.print("[yellow]No reasoning.jsonl found — session has not been annotated or no VLM was used.[/yellow]")
        raise typer.Exit(code=0)

    step_count = 0
    model_steps = 0
    # provider → first model_id seen (used for registry lookup)
    provider_model_ids: dict[str, str] = {}
    # For --detailed: list of step dicts
    detailed_steps: list[dict] = []
    # Fallback token accumulation from reasoning.jsonl when metadata.json lacks counts
    jsonl_input = 0
    jsonl_output = 0
    jsonl_per_provider: dict[str, dict[str, int]] = {}

    try:
        for line in jsonl_path.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                entry = _json.loads(line)
            except _json.JSONDecodeError:
                continue
            step_count += 1
            source = entry.get("source", "")
            if source in ("model", "model_degraded"):
                model_steps += 1
            provider_name: str = entry.get("provider") or ""
            model_id: str = entry.get("model_id") or ""
            if provider_name and model_id and provider_name not in provider_model_ids:
                provider_model_ids[provider_name] = model_id

            input_t: int = entry.get("input_tokens") or 0
            output_t: int = entry.get("output_tokens") or 0
            jsonl_input += input_t
            jsonl_output += output_t
            if input_t or output_t:
                bucket = jsonl_per_provider.setdefault(
                    provider_name, {"input": 0, "output": 0, "steps": 0}
                )
                bucket["input"] += input_t
                bucket["output"] += output_t
                bucket["steps"] += 1

            if detailed:
                detailed_steps.append({
                    "step_id": entry.get("step_id"),
                    "provider": provider_name,
                    "model_id": model_id,
                    "input_tokens": input_t,
                    "output_tokens": output_t,
                    "source": source,
                    "converted_command": entry.get("converted_command", ""),
                })
    except OSError as e:
        console.print(f"[red]Failed to read reasoning.jsonl: {e}[/red]")
        raise typer.Exit(code=1)

    # Use metadata.json totals if present; fall back to summed reasoning.jsonl.
    if total_input == 0 and total_output == 0:
        total_input = jsonl_input
        total_output = jsonl_output

    # Build per-provider token counts: prefer metadata.json (fast path),
    # fall back to jsonl accumulation.
    if meta_provider_counts:
        per_provider_counts: dict[str, tuple[int, int]] = {
            p: (v.get("input_tokens", 0), v.get("output_tokens", 0))
            for p, v in meta_provider_counts.items()
        }
    else:
        per_provider_counts = {
            p: (v["input"], v["output"])
            for p, v in jsonl_per_provider.items()
            if v["input"] or v["output"]
        }

    # Rate resolution per provider.
    # Order: pricing registry (by model_id) → ProviderConfig (by provider name) → None.
    cfg = settings.model
    provider_config_by_name = {p.name: p for p in cfg.providers}

    def _resolve_rates(provider_name: str) -> tuple[float | None, float | None, str]:
        """Return (input_per_million, output_per_million, source_label)."""
        mid = provider_model_ids.get(provider_name, "")
        if mid:
            rates = registry.lookup(mid)
            if rates:
                return rates[0], rates[1], f"registry({mid})"

        pconf = provider_config_by_name.get(provider_name)
        if pconf and (pconf.cost_per_million_input_tokens or pconf.cost_per_million_output_tokens):
            return (
                pconf.cost_per_million_input_tokens,
                pconf.cost_per_million_output_tokens,
                "config",
            )

        # Global flat config rates as last resort (single-provider sessions).
        if cfg.cost_per_million_input_tokens or cfg.cost_per_million_output_tokens:
            return (
                cfg.cost_per_million_input_tokens,
                cfg.cost_per_million_output_tokens,
                "config(global)",
            )

        return None, None, "unknown"

    # Compute total cost summed across providers.
    total_cost = 0.0
    provider_cost_rows: list[tuple[str, int, int, float, str]] = []
    for pname, (pin, pout) in sorted(
        per_provider_counts.items(), key=lambda x: -(x[1][0] + x[1][1])
    ):
        in_rate, out_rate, rate_src = _resolve_rates(pname)
        if in_rate is not None and out_rate is not None:
            pcost = (pin / 1_000_000) * in_rate + (pout / 1_000_000) * out_rate
        else:
            pcost = 0.0
        total_cost += pcost
        provider_cost_rows.append((pname or "(unknown)", pin, pout, pcost, rate_src))

    # If we have no per-provider breakdown, use global rates on aggregate.
    if not provider_cost_rows and (total_input or total_output):
        in_rate, out_rate, rate_src = _resolve_rates("")
        if in_rate is not None and out_rate is not None:
            total_cost = (
                (total_input / 1_000_000) * in_rate
                + (total_output / 1_000_000) * out_rate
            )
        rate_src_label = rate_src
    else:
        rate_src_label = ", ".join({r for *_, r in provider_cost_rows}) or "unknown"

    memory_name = record.get("memory_name", session[:8])
    console.print(f"\n[bold]Cost report: {memory_name}[/bold]")
    console.print(f"  Session      : {session}")
    console.print(f"  Steps        : {step_count} total, {model_steps} with VLM tokens")
    console.print(f"  Input tokens : {total_input:,}")
    console.print(f"  Output tokens: {total_output:,}")
    console.print(f"  Total tokens : {total_input + total_output:,}")
    console.print(f"  Rate source  : {rate_src_label}")
    console.print(f"  [bold]Estimated cost: ${total_cost:.6f}[/bold]")

    if len(provider_cost_rows) > 1 or (
        len(provider_cost_rows) == 1 and provider_cost_rows[0][0] != "(unknown)"
    ):
        console.print("\n  Per-provider breakdown:")
        for label, pin, pout, pcost, rate_src in provider_cost_rows:
            console.print(
                f"    {label:24s}  in={pin:,}  out={pout:,}  "
                f"cost=${pcost:.6f}  [{rate_src}]"
            )

    if detailed and detailed_steps:
        console.print("\n  Per-step breakdown:")
        for s in detailed_steps:
            sid = s["step_id"]
            prov = s["provider"] or "(unknown)"
            mid = s["model_id"] or "-"
            pin = s["input_tokens"]
            pout = s["output_tokens"]
            in_rate, out_rate, _ = _resolve_rates(s["provider"])
            if in_rate is not None and out_rate is not None:
                scost = (pin / 1_000_000) * in_rate + (pout / 1_000_000) * out_rate
                cost_str = f"${scost:.6f}"
            else:
                cost_str = "unknown"
            cmd = s["converted_command"][:40]
            console.print(
                f"    step {sid:>4}  {prov:20s}  {mid:30s}  "
                f"in={pin:,} out={pout:,}  {cost_str}  {cmd}"
            )

    if total_input == 0 and total_output == 0:
        console.print(
            "\n[dim]No token data found — session may be manual-only or "
            "VLM tokens were not recorded.[/dim]"
        )


@app.command()
def ping() -> None:
    """Check that ma-core is running and the IPC channel works."""
    from ma_app.ipc.client import IPCClient, IPCError
    try:
        with IPCClient() as client:
            version = client.ping()
        console.print(f"[green]ma-core is running — v{version}[/green]")
    except IPCError as e:
        console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)

# server subcommand group
server_app = typer.Typer(help="Manage the ma-core server process.")
app.add_typer(server_app, name="server")


def _find_ma_core_argv(release: bool) -> tuple[list[str], str]:
    """
    Resolve how to launch ma-core. Returns (argv, human_label).

    Priority:
      1. MA_CORE_BIN env var — explicit override, used as-is.
      2. 'ma-core' on PATH — installed release binary.
      3. Workspace target/release/ma-core or target/debug/ma-core —
         built but not installed; workspace root inferred from this file's
         location (ma-app/ma_app/cli.py → ../../ → workspace root).
      4. cargo run -p ma-core — always works from workspace root, slowest.
    """
    import os
    import shutil
    from pathlib import Path

    # 1. Explicit override
    env_bin = os.environ.get("MA_CORE_BIN", "").strip()
    if env_bin:
        return [env_bin], f"MA_CORE_BIN={env_bin}"

    import sys
    _exe = "ma-core.exe" if sys.platform == "win32" else "ma-core"

    # 2. On PATH
    if shutil.which(_exe):
        return [_exe], f"{_exe} (PATH)"

    # 3. Workspace-relative compiled binary
    # cli.py is at ma-app/ma_app/cli.py → parent×2 = ma-app/ → parent = workspace
    workspace = Path(__file__).resolve().parent.parent.parent
    tier = "release" if release else "debug"
    candidate = workspace / "target" / tier / _exe
    if candidate.exists():
        return [str(candidate)], str(candidate)

    # Also try the other tier as a fallback
    other_tier = "debug" if release else "release"
    other = workspace / "target" / other_tier / _exe
    if other.exists():
        return [str(other)], str(other)

    # 4. cargo run — requires being in (or passing) the workspace root
    cargo_argv = ["cargo", "run", "-p", "ma-core"]
    if release:
        cargo_argv.append("--release")
    return cargo_argv, "cargo run -p ma-core" + (" --release" if release else "")


@server_app.command("start")
def server_start(
    daemon: bool = typer.Option(
        False, "--daemon", help="Run ma-core in the background."
    ),
    log_file: str = typer.Option(
        "", "--log-file",
        help="Log file path for daemon mode (default: ~/.memory-archive/ma-core.log).",
    ),
    release: bool = typer.Option(
        True, "--release/--debug",
        help="Use the release binary (default: --release).",
    ),
) -> None:
    """Start the ma-core server."""
    import os
    import subprocess
    from pathlib import Path

    argv, label = _find_ma_core_argv(release)

    if not daemon:
        console.print(f"[green]Starting ma-core[/green] ({label})")
        if "cargo run" in label:
            console.print(
                "[yellow]No compiled binary found — building from source. "
                "This takes ~60s on first run.[/yellow]\n"
                "  To pre-build: cargo build --release -p ma-core\n"
            )
        console.print("[dim]Press Ctrl+C to stop.[/dim]\n")
        import sys
        if sys.platform != "win32":
            os.execvp(argv[0], argv)
            console.print(
                f"[red]Failed to exec ma-core.[/red]\n"
                f"  Tried: {argv[0]}\n"
                "  Build with: cargo build --release -p ma-core\n"
                "  Or set MA_CORE_BIN=/path/to/ma-core"
            )
            raise typer.Exit(code=1)
        else:
            try:
                proc = subprocess.run(argv)
                raise typer.Exit(code=proc.returncode)
            except FileNotFoundError:
                console.print(
                    f"[red]Failed to start ma-core.[/red]\n"
                    f"  Tried: {argv[0]}\n"
                    "  Build with: cargo build --release -p ma-core\n"
                    "  Or set MA_CORE_BIN=/path/to/ma-core"
                )
                raise typer.Exit(code=1)

    # Daemon mode
    log_path = Path(
        log_file.strip() or Path.home() / ".memory-archive" / "ma-core.log"
    )
    log_path.parent.mkdir(parents=True, exist_ok=True)

    pid_path = Path.home() / ".memory-archive" / "ma-core.pid"
    if pid_path.exists():
        try:
            existing_pid = int(pid_path.read_text().strip())
            # Send signal 0 — checks whether the process is alive.
            os.kill(existing_pid, 0)
            console.print(
                f"[yellow]ma-core is already running (PID {existing_pid}).[/yellow]\n"
                f"  Stop it first: memory-archive server stop"
            )
            raise typer.Exit(code=1)
        except (ProcessLookupError, PermissionError, ValueError):
            pass  # PID file stale — proceed

    import sys
    log_fh = open(log_path, "a")
    popen_kwargs: dict = dict(stdout=log_fh, stderr=log_fh)
    if sys.platform == "win32":
        popen_kwargs["creationflags"] = (
            subprocess.DETACHED_PROCESS | subprocess.CREATE_NEW_PROCESS_GROUP
        )
    else:
        popen_kwargs["start_new_session"] = True
    proc = subprocess.Popen(argv, **popen_kwargs)
    log_fh.close()

    console.print(
        f"[green]ma-core started in background[/green] (PID {proc.pid})\n"
        f"  Binary : {label}\n"
        f"  Logs   : {log_path}\n"
        f"  Stop   : memory-archive server stop\n"
        f"  Follow : memory-archive server logs --follow"
    )


@server_app.command("stop")
def server_stop() -> None:
    """Send SIGTERM to a running ma-core daemon."""
    import os
    import signal
    from pathlib import Path

    pid_path = Path.home() / ".memory-archive" / "ma-core.pid"

    if not pid_path.exists():
        console.print(
            "[yellow]No ma-core.pid file found.[/yellow]\n"
            "  If ma-core was started in the foreground, use Ctrl+C.\n"
            f"  Expected PID file at: {pid_path}"
        )
        raise typer.Exit(code=1)

    try:
        pid = int(pid_path.read_text().strip())
    except ValueError:
        console.print(f"[red]ma-core.pid contains invalid data: {pid_path}[/red]")
        raise typer.Exit(code=1)

    import sys
    try:
        if sys.platform == "win32":
            ret = subprocess.run(
                ["taskkill", "/PID", str(pid), "/F"],
                capture_output=True,
            )
            if ret.returncode == 0:
                console.print(f"[green]ma-core (PID {pid}) terminated.[/green]")
                console.print("  Active sessions will be flagged as interrupted before exit.")
            else:
                console.print(
                    f"[yellow]taskkill failed for PID {pid} — ma-core may not be running.[/yellow]\n"
                    f"  Stale PID file removed."
                )
                pid_path.unlink(missing_ok=True)
                raise typer.Exit(code=1)
        else:
            os.kill(pid, signal.SIGTERM)
            console.print(f"[green]SIGTERM sent to ma-core (PID {pid}).[/green]")
            console.print("  Active sessions will be flagged as interrupted before exit.")
    except ProcessLookupError:
        console.print(
            f"[yellow]No process with PID {pid} — ma-core is not running.[/yellow]\n"
            f"  Stale PID file removed."
        )
        pid_path.unlink(missing_ok=True)
        raise typer.Exit(code=1)
    except PermissionError:
        console.print(
            f"[red]Permission denied terminating PID {pid}.[/red]\n"
            f"  Try running as administrator."
        )
        raise typer.Exit(code=1)


@server_app.command("logs")
def server_logs(
    lines: int = typer.Option(50, "--lines", "-n", help="Number of recent lines to show."),
    follow: bool = typer.Option(False, "--follow", "-f", help="Stream new log output (like tail -f)."),
    log_file: str = typer.Option(
        "", "--log-file", help="Log file path (default: ~/.memory-archive/ma-core.log)."
    ),
) -> None:
    """Show or stream ma-core daemon logs."""
    import subprocess
    from pathlib import Path

    log_path = Path(
        log_file.strip() or Path.home() / ".memory-archive" / "ma-core.log"
    )

    if not log_path.exists():
        console.print(
            f"[yellow]Log file not found: {log_path}[/yellow]\n"
            "  ma-core may have been started in foreground mode (logs go to stdout).\n"
            "  Start as daemon with: memory-archive server start --daemon"
        )
        raise typer.Exit(code=1)

    import sys
    import time
    import shutil
    if sys.platform != "win32" and shutil.which("tail"):
        tail_argv = ["tail", f"-n{lines}"] + (["-f"] if follow else []) + [str(log_path)]
        try:
            subprocess.run(tail_argv)
        except KeyboardInterrupt:
            pass
    else:
        try:
            with open(log_path, "r", encoding="utf-8", errors="replace") as fh:
                all_lines = fh.readlines()
            for line in all_lines[-lines:]:
                console.print(line, end="")
            if follow:
                console.print("[dim](following — press Ctrl+C to stop)[/dim]")
                with open(log_path, "r", encoding="utf-8", errors="replace") as fh:
                    fh.seek(0, 2)
                    while True:
                        chunk = fh.read(4096)
                        if chunk:
                            console.print(chunk, end="")
                        else:
                            time.sleep(0.25)
        except KeyboardInterrupt:
            pass

@server_app.command("kafka-bridge")
def server_kafka_bridge(
    session_id: str = typer.Option(
        ..., "--session", "-s",
        help="Session ID this bridge will forward events for.",
    ),
    cc_addr: str = typer.Option(
        "", "--cc-addr",
        help="Control-Center gRPC address (default: from config control_center_addr).",
    ),
    kafka_broker: str = typer.Option(
        "", "--kafka-broker",
        help="Kafka broker address (default: from config kafka_broker).",
    ),
    release: bool = typer.Option(
        True, "--release/--debug",
        help="Use the release binary (default: --release).",
    ),
) -> None:
    """
    Start the ma-kafka-producer bridge for one session.

    Connects to Control-Center's WatchCommands gRPC stream and forwards
    every event to the Kafka 'control-center-events' topic. Required for
    cloud_primary mode — not needed for local mode.

    Runs until the Control-Center stream ends or Ctrl+C is pressed.
    """
    import os
    import shutil
    import subprocess
    from pathlib import Path

    from ma_app.config.settings import Settings
    settings = Settings.load()

    resolved_cc = cc_addr.strip() or settings.control_center_addr.strip()
    resolved_broker = kafka_broker.strip() or settings.kafka_broker.strip()

    if not resolved_cc:
        console.print(
            "[red]No Control-Center address.[/red]\n"
            "  Pass --cc-addr or set it: memory-archive config --control-center-addr <addr>"
        )
        raise typer.Exit(code=1)

    if not resolved_broker:
        console.print(
            "[red]No Kafka broker address.[/red]\n"
            "  Pass --kafka-broker or set it: memory-archive config --kafka-broker <addr>"
        )
        raise typer.Exit(code=1)

    # Resolve the ma-kafka-producer binary — same priority order as ma-core.
    env_bin = os.environ.get("MA_KAFKA_PRODUCER_BIN", "").strip()
    if env_bin:
        argv = [env_bin]
        label = f"MA_KAFKA_PRODUCER_BIN={env_bin}"
    elif shutil.which("ma-kafka-producer"):
        argv = ["ma-kafka-producer"]
        label = "ma-kafka-producer (PATH)"
    else:
        workspace = Path(__file__).resolve().parent.parent.parent
        tier = "release" if release else "debug"
        candidate = workspace / "target" / tier / "ma-kafka-producer"
        other = workspace / "target" / ("debug" if release else "release") / "ma-kafka-producer"
        if candidate.exists():
            argv = [str(candidate)]
            label = str(candidate)
        elif other.exists():
            argv = [str(other)]
            label = str(other)
        else:
            argv = ["cargo", "run", "-p", "ma-kafka-producer"] + (["--release"] if release else [])
            label = "cargo run -p ma-kafka-producer" + (" --release" if release else "")

    full_argv = argv + [
        "--cc-addr",      resolved_cc,
        "--kafka-broker", resolved_broker,
        "--session-id",   session_id,
    ]

    console.print(
        f"[green]Starting ma-kafka-producer[/green] ({label})\n"
        f"  Session : {session_id}\n"
        f"  CC addr : {resolved_cc}\n"
        f"  Broker  : {resolved_broker}\n"
        "[dim]Streaming events to Kafka. Press Ctrl+C to stop.[/dim]\n"
    )

    try:
        subprocess.run(full_argv)
    except KeyboardInterrupt:
        console.print("\n[yellow]Kafka bridge stopped.[/yellow]")
    except FileNotFoundError:
        console.print(
            f"[red]Binary not found: {full_argv[0]}[/red]\n"
            "  Build with: cargo build --release -p ma-kafka-producer\n"
            "  Or set MA_KAFKA_PRODUCER_BIN=/path/to/ma-kafka-producer"
        )
        raise typer.Exit(code=1)
    
@app.command("version")
def version_command() -> None:
    """Show the installed version of Memory Archive."""
    console.print(f"[bold]memory-archive[/bold] {__version__}")

# Register update / uninstall commands
from ma_app.updater import register_commands
register_commands(app)

# Package entrypoint
if __name__ == "__main__":
    app()