# ma-app/ma_app/updater.py
# Self-update and uninstall commands for the `memory-archive` CLI.

from __future__ import annotations

import json
import os
import platform
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import NoReturn, Optional

import typer
from rich.console import Console

try:
    from ma_app import __version__
except ImportError:
    __version__ = "0.13.0"

# =============================================================================
# Constants
# =============================================================================

REPO_OWNER   = "nullvoider07"
REPO_NAME    = "memory-archive"
REPO_URL     = f"https://github.com/{REPO_OWNER}/{REPO_NAME}"
RELEASES_API = (
    f"https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest"
)

# Rust binaries shipped inside every platform archive.
# ma-proto is a compile-time library — it has no standalone binary.
RUST_BINARIES = ["ma-core", "ma-kafka-producer"]

# Names this tool is known by. These are DIFFERENT and must not be conflated:
#   - the pip distribution (what `pip show` / `pip uninstall` recognise) is
#     "ma-app" — see ma-app/pyproject.toml `name`;
#   - the CLI launcher installed on PATH is the `memory-archive` entry-point
#     script (pyproject `[project.scripts]`).
PIP_DIST_NAME    = "ma-app"
CLI_COMMAND_NAME = "memory-archive"

# Config directory written by ma-core at first run.
CONFIG_DIR = Path.home() / ".memory-archive"

_console = Console()


# =============================================================================
# Internal helpers
# =============================================================================

def _abort(message: str) -> NoReturn:
    """Print a red error message and exit with code 1."""
    _console.print(f"[red]  [XX] ERROR: {message}[/red]")
    raise typer.Exit(code=1)


def _os_info() -> tuple[str, str]:
    """Return (os_name, arch) normalised to the artifact naming convention."""
    os_type = platform.system().lower()
    machine = platform.machine().lower()

    if os_type == "darwin":
        os_name = "macos"
    elif os_type == "linux":
        os_name = "linux"
    elif os_type == "windows":
        os_name = "windows"
    else:
        _abort(f"Unsupported operating system: {os_type}.")

    if machine in ("x86_64", "amd64"):
        arch = "x64"
    elif machine in ("aarch64", "arm64"):
        arch = "arm64"
    else:
        _abort(f"Unsupported CPU architecture: {machine}.")

    return os_name, arch  # type: ignore[return-value]  # _abort() exits


def _artifact_name(os_name: str, arch: str) -> str:
    """Return the release asset filename for this platform."""
    ext = "zip" if os_name == "windows" else "tar.gz"
    return f"ma-{os_name}-{arch}.{ext}"

def _is_admin_windows() -> bool:
    """Return True when the current process has administrator privileges."""
    try:
        import ctypes
        return bool(ctypes.windll.shell32.IsUserAnAdmin())
    except Exception:
        return False

def _install_dirs(os_name: str) -> tuple[Path, Path]:
    """
    Return (binary_dir, data_dir) matching the paths chosen by install.sh
    and install.ps1 so that `update` replaces files in the same locations.

    Priority:
      1. Wherever the current `ma-core` binary lives (most reliable).
      2. System-wide /usr/local/bin when running as root (Linux/macOS).
      3. Per-user ~/.local/bin (Linux/macOS) or %LOCALAPPDATA%/MemoryArchive
         (Windows).
    """
    if os_name == "windows":
        local_app_data = Path(os.environ.get("LOCALAPPDATA", str(Path.home())))
        ma_home      = (
            Path(os.environ.get("PROGRAMFILES", "C:\\Program Files")) / "MemoryArchive"
            if _is_admin_windows()
            else local_app_data / "MemoryArchive"
        )
    else:
        ma_home = (
            Path("/opt/memory-archive")
            if os.geteuid() == 0
            else Path.home() / ".memory-archive"
        )

    default_bin  = ma_home / "bin"
    default_data = ma_home / "share"

    # If ma-core is already installed somewhere, use that location instead.
    existing = shutil.which("ma-core") or shutil.which("ma-core.exe")
    bin_dir  = Path(existing).parent if existing else default_bin

    return bin_dir, default_data


def _fetch_release_info() -> dict:
    """Query GitHub Releases API. Tries requests first, falls back to urllib."""
    headers = {
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    try:
        import requests as _req
        r = _req.get(RELEASES_API, headers=headers, timeout=10)
        r.raise_for_status()
        return r.json()
    except ImportError:
        import urllib.request
        req = urllib.request.Request(RELEASES_API, headers=headers)
        with urllib.request.urlopen(req, timeout=10) as resp:
            return json.loads(resp.read().decode())


def _download_with_curl(url: str, dest: Path) -> None:
    """
    Download url to dest using curl so the tabular progress stats are shown.
    Falls back to urllib if curl is unavailable (logs a warning).
    """
    curl = shutil.which("curl") or shutil.which("curl.exe")
    if curl:
        cmd = [curl, "-fL", "--progress-bar", "-o", str(dest)]
        token = os.environ.get("GITHUB_TOKEN")
        if token:
            cmd += ["-H", f"Authorization: Bearer {token}",
                    "-H", "Accept: application/octet-stream"]
        cmd.append(url)
        result = subprocess.run(cmd, check=False)
        if result.returncode != 0:
            _abort(f"curl exited with code {result.returncode}.\nURL: {url}")
    else:
        _console.print(
            "[yellow]  [!!] curl not found — downloading silently via urllib.[/yellow]"
        )
        import urllib.request
        urllib.request.urlretrieve(url, dest)


def _verify_archive_checksum(archive_path: Path, artifact: str, tag: str) -> None:
    """Verify archive_path against the release SHA256SUMS.

    Hard-fails (via _abort) if SHA256SUMS is missing, has no entry for the
    artifact, or the hash does not match — refusing to install an unverified or
    tampered artifact (prevents strip/downgrade attacks).
    """
    import hashlib
    import urllib.request

    sums_url = f"{REPO_URL}/releases/download/{tag}/SHA256SUMS"
    try:
        with urllib.request.urlopen(sums_url, timeout=15) as resp:
            sums_text = resp.read().decode("utf-8", "replace")
    except Exception as e:  # noqa: BLE001 — any failure is a hard stop
        _abort(
            f"SHA256SUMS not found for {tag}: {e}\n"
            "  Refusing to install unverified artifacts."
        )

    expected: Optional[str] = None
    for line in sums_text.splitlines():
        parts = line.split()
        if len(parts) >= 2:
            name = parts[1].strip().lstrip("*")   # tolerate binary-mode '*' prefix
            if name == artifact:
                expected = parts[0].strip().lower()
                break
    if not expected:
        _abort(f"No checksum entry for {artifact} in SHA256SUMS. Refusing to install.")

    h = hashlib.sha256()
    with open(archive_path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    actual = h.hexdigest().lower()

    if actual != expected:
        _abort(
            f"Checksum mismatch for {artifact} — possible tampering.\n"
            f"  expected: {expected}\n"
            f"  actual  : {actual}"
        )
    _console.print("[green]  [OK] Checksum verified (SHA-256)[/green]")


def _within(dest: Path, target: Path) -> bool:
    """Return True if target resolves to a path inside dest (traversal guard)."""
    dest_r = dest.resolve()
    try:
        target.resolve().relative_to(dest_r)
        return True
    except ValueError:
        return False


def _extract_archive(archive: Path, dest: Path, os_name: str) -> None:
    """Safely extract a flat .tar.gz (Linux/macOS) or .zip (Windows) into dest.

    Every member is validated before writing: absolute paths, ``..`` components,
    and tar sym/hardlinks pointing outside ``dest`` are rejected. This prevents
    a crafted archive from writing outside the destination (CVE-2007-4559 class).
    """
    dest = dest.resolve()
    if os_name == "windows":
        import zipfile
        with zipfile.ZipFile(archive, "r") as zf:
            for name in zf.namelist():
                p = Path(name)
                if p.is_absolute() or ".." in p.parts or not _within(dest, dest / name):
                    _abort(f"Unsafe path in archive, refusing to extract: {name!r}")
            zf.extractall(dest)
    else:
        import tarfile
        with tarfile.open(archive, "r:gz") as tf:
            for m in tf.getmembers():
                mp = Path(m.name)
                if mp.is_absolute() or ".." in mp.parts or not _within(dest, dest / m.name):
                    _abort(f"Unsafe path in archive, refusing to extract: {m.name!r}")
                if m.issym() or m.islnk():
                    # Validate the link *target*, resolved relative to the member's
                    # own directory (symlink) or the archive root (hardlink).
                    if m.islnk():
                        link_target = dest / m.linkname
                    else:
                        link_target = dest / mp.parent / m.linkname
                    if Path(m.linkname).is_absolute() or not _within(dest, link_target):
                        _abort(f"Unsafe link in archive, refusing to extract: {m.name!r} -> {m.linkname!r}")
            tf.extractall(dest)


def _chmod_exec(path: Path) -> None:
    """Add executable bits (owner + group + other) to a file."""
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def _macos_finalize(binary: Path) -> None:
    """
    After replacing a binary on macOS:
      1. Remove the quarantine xattr Gatekeeper adds to downloaded files.
      2. Re-apply ad-hoc codesign so the binary passes Gatekeeper checks.
    Matches what install.sh does on first install.
    """
    subprocess.run(
        ["xattr", "-d", "com.apple.quarantine", str(binary)],
        check=False, capture_output=True,
    )
    subprocess.run(
        ["codesign", "--force", "--deep", "--sign", "-", str(binary)],
        check=False, capture_output=True,
    )


def _replace_binary_windows(src: Path, dst: Path) -> None:
    """
    Windows cannot overwrite an in-use executable.  Strategy:
      1. Rename the old binary to <n>.old (atomic on NTFS).
      2. Copy the new binary into place.
      3. Schedule deferred deletion of <n>.old via ping+del (fires ~3 s
         after this process exits, when the file handle is released).
    """
    old = dst.with_suffix(dst.suffix + ".old")
    if old.exists():
        try:
            old.unlink()
        except OSError:
            pass
    if dst.exists():
        try:
            dst.rename(old)
            cmd = f'cmd /c ping 127.0.0.1 -n 3 > nul & del "{old}"'
            subprocess.Popen(
                cmd,
                shell=True,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            )
        except PermissionError:
            _abort(
                f"Cannot replace {dst.name} — the file is in use.\n"
                "  Stop all running Memory Archive processes and try again."
            )
    shutil.copy2(src, dst)


def _pip_install_wheel(whl: Path, bin_dir: Path) -> bool:
    """
    Install the wheel into MA_HOME/lib using --prefix, then copy the
    memory-archive entry point into bin_dir alongside the Rust binaries.
    """
    os_name, _ = _os_info()
    ma_home  = bin_dir.parent          # bin_dir is MA_HOME/bin
    lib_dir  = ma_home / "lib"
    lib_dir.mkdir(parents=True, exist_ok=True)

    # pip's resolver output ("Requirement already satisfied" lines) is captured
    # and surfaced only on failure so it does not flood the display; a Rich
    # status spinner shows liveness while pip runs.
    with _console.status(f"[cyan]Reinstalling {whl.name} …", spinner="dots"):
        result = subprocess.run(
            [
                sys.executable, "-m", "pip", "install",
                "--prefix", str(lib_dir),
                "--no-warn-script-location",
                "--disable-pip-version-check",
                "--quiet",
                "--upgrade",
                str(whl),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
    if result.returncode != 0:
        out = (result.stdout or "").rstrip()
        err = (result.stderr or "").rstrip()
        if out:
            _console.print(out)
        if err:
            _console.print(f"[red]{err}[/red]")
        return False

    # Copy the entry point into bin_dir so only one directory is on PATH.
    scripts_dir = lib_dir / ("Scripts" if os_name == "windows" else "bin")
    ep_name     = "memory-archive.exe" if os_name == "windows" else "memory-archive"
    ep_src      = scripts_dir / ep_name
    ep_dst      = bin_dir / ep_name

    if ep_src.exists():
        shutil.copy2(ep_src, ep_dst)
        if os_name != "windows":
            _chmod_exec(ep_dst)
            # Patch shebang to the current interpreter.
            _patch_shebang(ep_dst)
    return True


def _patch_shebang(script: Path) -> None:
    """Replace the first line of a Python entry point with the current interpreter."""
    try:
        text  = script.read_text(encoding="utf-8")
        lines = text.splitlines(keepends=True)
        if lines and lines[0].startswith("#!"):
            lines[0] = f"#!{sys.executable}\n"
            script.write_text("".join(lines), encoding="utf-8")
    except Exception:
        pass


def _remove_path_from_shell_rc(install_dir: Path) -> None:
    """
    Remove the PATH export line that install.sh wrote to the user's shell rc
    file.  Looks for the sentinel comment written by the installer:
        # Added by Memory Archive installer
    and removes it along with the export/fish_add_path line below it.
    Safe to call when the sentinel is absent — no file is modified.
    """
    zdotdir = os.environ.get("ZDOTDIR")
    rc_candidates = [
        Path.home() / ".bashrc",
        Path.home() / ".zshrc",
        Path.home() / ".config" / "fish" / "config.fish",
    ]
    if zdotdir:
        rc_candidates.insert(0, Path(zdotdir) / ".zshrc")

    sentinel = "# Added by Memory Archive installer"

    for rc in rc_candidates:
        if not rc.exists():
            continue
        text = rc.read_text(encoding="utf-8", errors="replace")
        if sentinel not in text and str(install_dir) not in text:
            continue

        lines     = text.splitlines(keepends=True)
        new_lines: list[str] = []
        skip_next = False
        changed   = False

        for line in lines:
            if skip_next:
                skip_next = False
                changed   = True
                continue
            if sentinel in line:
                skip_next = True
                changed   = True
                continue
            if str(install_dir) in line and (
                "export PATH" in line or "fish_add_path" in line
            ):
                changed = True
                continue
            new_lines.append(line)

        if changed:
            rc.write_text("".join(new_lines), encoding="utf-8")
            _console.print(f"[green]  [OK] Removed PATH entry from {rc}[/green]")


# ═════════════════════════════════════════════════════════════════════════════
# UPDATE COMMAND  (private function — registered via register_commands)
# ═════════════════════════════════════════════════════════════════════════════

def _update_command(
    check_only: bool = typer.Option(
        False,
        "--check-only",
        help="Check whether a newer version exists without downloading anything.",
    ),
) -> None:
    """Check for updates and install the latest version.

    \b
    Examples:
        memory-archive update               # Check and install updates
        memory-archive update --check-only  # Only check, do not install
    """
    os_name, arch = _os_info()

    # 1. Fetch release metadata
    _console.print("Checking for updates …")
    _console.print(f"  Current version : v{__version__}")

    try:
        release = _fetch_release_info()
    except Exception as exc:
        _abort(
            f"Failed to reach GitHub API: {exc}\n"
            f"  Check your internet connection and try again.\n"
            f"  Manual download: {REPO_URL}/releases/latest"
        )

    latest_tag     = release["tag_name"]        # type: ignore[index]
    latest_version = latest_tag.lstrip("v")

    _console.print(f"  Latest version  : {latest_tag}")

    # 2. Version comparison
    if latest_version == __version__:
        _console.print(
            "\n[green]  [OK] You already have the latest version.[/green]"
        )
        return

    _console.print(
        f"\n[yellow]  [->] New version available: {latest_tag}[/yellow]"
    )

    if check_only:
        _console.print("\nTo install the update, run:")
        _console.print("  memory-archive update")
        return

    # 3. Confirm
    if not typer.confirm("\nDo you want to update now?"):
        _console.print("Update cancelled.")
        raise typer.Exit()

    # 4. Resolve artifact + download URL
    artifact     = _artifact_name(os_name, arch)
    download_url: Optional[str] = None

    for asset in release.get("assets", []):     # type: ignore[union-attr]
        if asset.get("name") == artifact:
            download_url = asset["browser_download_url"]
            break

    if not download_url:
        # Construct the URL directly when the API manifest is incomplete
        # (e.g. the release was just created and assets are still uploading).
        download_url = f"{REPO_URL}/releases/download/{latest_tag}/{artifact}"
        _console.print(
            "[yellow]  [!!] Asset not in API manifest — using direct URL.[/yellow]"
        )

    # 5. Download
    tmp_dir = Path(tempfile.mkdtemp(prefix="ma-update-"))
    try:
        archive_path = tmp_dir / artifact

        _console.print(f"\nDownloading {artifact} …")
        _console.print("")    # blank line before the progress bar

        _download_with_curl(download_url, archive_path)

        size_mb = archive_path.stat().st_size / (1024 * 1024)
        _console.print(
            f"[green]  [OK] Downloaded ({size_mb:.1f} MB)[/green]"
        )

        # 5b. Verify artifact integrity before touching its contents.
        _verify_archive_checksum(archive_path, artifact, latest_tag)

        # 6. Extract
        _console.print("Extracting archive …")
        extract_dir = tmp_dir / "extracted"
        extract_dir.mkdir()
        _extract_archive(archive_path, extract_dir, os_name)

        # All files are at the archive root — verify the core binary is there.
        expected_core = "ma-core.exe" if os_name == "windows" else "ma-core"
        if not (extract_dir / expected_core).exists():
            _abort(
                f"{expected_core} not found in {artifact}.\n"
                "  The release archive may be malformed.\n"
                f"  Please open an issue: {REPO_URL}/issues"
            )

        _console.print("[green]  [OK] Extraction complete[/green]")

        # 7. Install Rust binaries
        bin_dir, data_dir = _install_dirs(os_name)
        bin_dir.mkdir(parents=True, exist_ok=True)

        _console.print(f"\nInstalling binaries to {bin_dir} …")

        is_windows = os_name == "windows"
        is_macos   = os_name == "macos"

        for bin_name in RUST_BINARIES:
            src_name = f"{bin_name}.exe" if is_windows else bin_name
            src = extract_dir / src_name
            dst = bin_dir / src_name

            if not src.exists():
                _abort(
                    f"Expected binary not found in archive: {src_name}.\n"
                    "  The release may be incomplete.\n"
                    f"  Please open an issue: {REPO_URL}/issues"
                )

            if is_windows:
                _replace_binary_windows(src, dst)
            else:
                shutil.copy2(src, dst)
                _chmod_exec(dst)
                if is_macos:
                    _macos_finalize(dst)

            _console.print(f"[green]  [OK] Updated {src_name}[/green]")

        # 8. Reinstall Python wheel (ma-app). The archive ships one wheel named
        # ma_app-<ver>-...whl; match by extension (as install.sh does) so a
        # distribution rename can never silently skip the Python update.
        whl_matches = list(extract_dir.glob("*.whl"))
        if whl_matches:
            whl = whl_matches[0]
            _console.print("")   # spacing before the Python package step
            if _pip_install_wheel(whl, bin_dir):
                _console.print(
                    f"[green]  [OK] {PIP_DIST_NAME} updated[/green]"
                )
            else:
                _console.print(
                    f"[yellow]  [!!] pip install returned an error.\n"
                    f"       Run manually: pip install --upgrade \"{whl}\"[/yellow]"
                )
        else:
            _console.print(
                "[yellow]  [!!] No .whl found in archive — "
                "skipping Python package update.[/yellow]"
            )

        # 9. Refresh documentation in data dir 
        data_dir.mkdir(parents=True, exist_ok=True)
        for doc in ("LICENSE", "README.md", "INSTALL.md"):
            src_doc = extract_dir / doc
            if src_doc.exists():
                shutil.copy2(src_doc, data_dir / doc)

    except SystemExit:
        raise
    except Exception as exc:
        _abort(f"Update failed unexpectedly: {exc}")
    finally:
        shutil.rmtree(tmp_dir, ignore_errors=True)

    # 10. Success
    _console.print("")
    _console.print("=" * 60)
    _console.print(
        f"[green bold]  Successfully updated to {latest_tag}![/green bold]"
    )
    _console.print("=" * 60)
    _console.print(
        "\n  Restart any running ma-core processes to load the new version."
    )
    _console.print(f"  Changelog: {REPO_URL}/releases/tag/{latest_tag}\n")


# ═════════════════════════════════════════════════════════════════════════════
# UNINSTALL COMMAND  (private function — registered via register_commands)
# ═════════════════════════════════════════════════════════════════════════════

def _uninstall_command(
    purge: bool = typer.Option(
        False,
        "--purge",
        help=(
            "Also remove ~/.memory-archive/ "
            "(config, TLS certs, session data)."
        ),
    ),
    yes: bool = typer.Option(
        False,
        "--yes", "-y",
        help="Skip the confirmation prompt.",
    ),
) -> None:
    """Uninstall Memory Archive from your system.

    \b
    By default only the installed binaries and the Python package are removed.
    Configuration, TLS certificates, and session data in ~/.memory-archive/
    are preserved unless --purge is passed.

    \b
    Examples:
        memory-archive uninstall           # Remove binaries + CLI only
        memory-archive uninstall --purge   # Remove everything including config
        memory-archive uninstall -y        # Skip confirmation
    """
    os_name, _ = _os_info()
    is_windows  = os_name == "windows"

    _console.print("=" * 60)
    _console.print("  Memory Archive — Uninstall")
    _console.print("=" * 60)
    _console.print("")

    # 1. Discover installed files
    _console.print("Scanning for installed components …")

    bin_dir, data_dir = _install_dirs(os_name)

    bins_to_remove: list[Path] = []
    docs_to_remove: list[Path] = []
    cfg_to_remove:  list[Path] = []

    # Executables to remove: the Rust binaries PLUS the `memory-archive` CLI
    # launcher.  install.sh copies the launcher (a pip entry-point script) into
    # INSTALL_DIR alongside the binaries; that copy is not tracked by pip, so it
    # must be discovered and removed explicitly, or the command stays on PATH
    # after uninstall.
    removable_names = [*RUST_BINARIES, CLI_COMMAND_NAME]

    # Check the computed install dir AND every directory on PATH so we catch
    # installs done outside the default location and *all* copies of a launcher
    # (INSTALL_DIR plus the pip scripts dir) — shutil.which only returns the
    # first PATH match, which would leave a shadowed second copy behind.  Only
    # files named exactly like our executables are ever removed, so scanning
    # shared dirs is safe.
    binary_search_dirs: set[Path] = {bin_dir}
    for b in removable_names:
        found = shutil.which(b) or shutil.which(f"{b}.exe")
        if found:
            binary_search_dirs.add(Path(found).parent)
    for entry in os.environ.get("PATH", "").split(os.pathsep):
        entry = entry.strip()
        if entry:
            binary_search_dirs.add(Path(entry))

    for search_dir in binary_search_dirs:
        for b in removable_names:
            name = f"{b}.exe" if is_windows else b
            p    = search_dir / name
            if p.exists() and p not in bins_to_remove:
                bins_to_remove.append(p)
            # Leftover .old files from a previous Windows update.
            old = search_dir / (name + ".old")
            if old.exists() and old not in bins_to_remove:
                bins_to_remove.append(old)

    # Documentation directory
    if data_dir.exists():
        docs_to_remove.append(data_dir)

    # Python package installed by `memory-archive update` into MA_HOME/lib via
    # `pip install --prefix`.  That install is invisible to `pip show` here, so
    # it is removed explicitly.  Guard on the MA_HOME directory name so a
    # non-standard install location (e.g. ma-core copied into /usr/local/bin)
    # can never turn this into a delete of a shared system lib directory.
    install_home = bin_dir.parent
    if install_home.name in {".memory-archive", "memory-archive", "MemoryArchive"}:
        lib_dir = install_home / "lib"
        if lib_dir.exists() and lib_dir not in docs_to_remove:
            docs_to_remove.append(lib_dir)

    # Config + TLS certs (--purge only)
    # ~/.memory-archive/ holds config.json, CA cert/key, IPC cert/key, PID files.
    # Only removed when explicitly requested with --purge.
    if purge and CONFIG_DIR.exists():
        cfg_to_remove.append(CONFIG_DIR)

    # 2. Print what will be removed
    _console.print("")
    _console.print("[yellow bold]  Binaries & CLI launcher:[/yellow bold]")
    if bins_to_remove:
        for p in bins_to_remove:
            _console.print(f"    - {p}")
    else:
        _console.print("    - None found")

    _console.print("")
    _console.print("[yellow bold]  Python package:[/yellow bold]")
    pip_check = subprocess.run(
        [sys.executable, "-m", "pip", "show", PIP_DIST_NAME],
        capture_output=True, text=True, check=False,
    )
    pip_installed = pip_check.returncode == 0
    if pip_installed:
        location_line = next(
            (l for l in pip_check.stdout.splitlines() if l.startswith("Location:")),
            None,
        )
        location = location_line.split(":", 1)[1].strip() if location_line else "unknown"
        _console.print(f"    - {PIP_DIST_NAME}  (installed at {location})")
    else:
        _console.print(f"    - {PIP_DIST_NAME} not found via pip")

    _console.print("")
    _console.print("[yellow bold]  Program files:[/yellow bold]")
    if docs_to_remove:
        for p in docs_to_remove:
            _console.print(f"    - {p}")
    else:
        _console.print("    - None found")

    if purge:
        _console.print("")
        _console.print(
            "[yellow bold]  Configuration & TLS data (--purge):[/yellow bold]"
        )
        if cfg_to_remove:
            for p in cfg_to_remove:
                _console.print(f"    - {p}")
        else:
            _console.print("    - None found")

    # 3. Disk space summary
    all_paths  = bins_to_remove + docs_to_remove + cfg_to_remove
    total_bytes = 0
    for p in all_paths:
        if p.exists():
            if p.is_file():
                total_bytes += p.stat().st_size
            elif p.is_dir():
                total_bytes += sum(
                    f.stat().st_size for f in p.rglob("*") if f.is_file()
                )
    if total_bytes > 0:
        _console.print("")
        _console.print(
            f"  Disk space to be freed: {total_bytes / (1024 * 1024):.2f} MB"
        )

    # 4. Nothing to do?
    if (
        not bins_to_remove
        and not pip_installed
        and not docs_to_remove
        and not cfg_to_remove
    ):
        _console.print("")
        _console.print(
            "[green]  Memory Archive is not installed on this system.[/green]"
        )
        return

    # 5. Confirm
    _console.print("")
    if purge:
        _console.print(
            "[red bold]  WARNING: --purge will permanently delete your configuration,\n"
            "  TLS certificates and any local session data in ~/.memory-archive/.\n"
            "  This cannot be undone.[/red bold]"
        )
    else:
        _console.print("[red bold]  This action cannot be undone![/red bold]")
        if CONFIG_DIR.exists():
            _console.print(
                f"[cyan]  Note: {CONFIG_DIR} will be preserved.\n"
                "  Use --purge to also remove config and TLS data.[/cyan]"
            )

    if not yes:
        _console.print("")
        if not typer.confirm("  Do you want to continue?"):
            _console.print("\nUninstall cancelled.")
            raise typer.Exit()

    _console.print("\nUninstalling …\n")

    removed: list[str]             = []
    failed:  list[tuple[str, str]] = []

    # 6. Remove Rust binaries
    for p in bins_to_remove:
        if not p.exists():
            continue
        try:
            if is_windows:
                # Deferred delete so a running ma-core does not block removal.
                marked = p.with_suffix(p.suffix + ".delete_me")
                if marked.exists():
                    try: marked.unlink()
                    except OSError: pass
                p.rename(marked)
                cmd = f'cmd /c ping 127.0.0.1 -n 3 > nul & del "{marked}"'
                subprocess.Popen(
                    cmd, shell=True,
                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
                )
                removed.append(str(p))
                _console.print(
                    f"[green]  [OK] Scheduled for deletion: {p.name}[/green]"
                )
                continue
            else:
                p.unlink()

            removed.append(str(p))
            _console.print(f"[green]  [OK] Removed: {p}[/green]")

        except PermissionError:
            hint = "" if is_windows else "\n  Tip: re-run with sudo for system-wide paths."
            failed.append((str(p), f"Permission denied.{hint}"))
            _console.print(f"[red]  [XX] Failed: {p}  (permission denied)[/red]")
        except Exception as exc:
            failed.append((str(p), str(exc)))
            _console.print(f"[red]  [XX] Failed: {p}  ({exc})[/red]")

    # 7. Uninstall Python package via pip
    if pip_installed:
        _console.print(f"\n  Uninstalling Python package ({PIP_DIST_NAME}) …")
        result = subprocess.run(
            [
                sys.executable, "-m", "pip", "uninstall",
                "--yes", "--quiet", PIP_DIST_NAME,
            ],
            check=False,
        )
        if result.returncode == 0:
            removed.append(PIP_DIST_NAME)
            _console.print(
                f"[green]  [OK] pip uninstall {PIP_DIST_NAME}[/green]"
            )
        else:
            failed.append(
                (PIP_DIST_NAME, f"pip uninstall exited {result.returncode}")
            )
            _console.print(
                f"[yellow]  [!!] pip uninstall failed (exit {result.returncode}).\n"
                f"       Run manually: pip uninstall {PIP_DIST_NAME}[/yellow]"
            )

    # 8. Remove documentation directory
    for p in docs_to_remove:
        try:
            shutil.rmtree(p) if p.is_dir() else p.unlink()
            removed.append(str(p))
            _console.print(f"[green]  [OK] Removed: {p}[/green]")
        except Exception as exc:
            failed.append((str(p), str(exc)))
            _console.print(f"[red]  [XX] Failed: {p}  ({exc})[/red]")

    # 9. Remove config / TLS data (--purge only)
    for p in cfg_to_remove:
        try:
            shutil.rmtree(p) if p.is_dir() else p.unlink()
            removed.append(str(p))
            _console.print(f"[green]  [OK] Removed: {p}[/green]")
        except Exception as exc:
            failed.append((str(p), str(exc)))
            _console.print(f"[red]  [XX] Failed: {p}  ({exc})[/red]")

    # 10. Platform-specific housekeeping
    if is_windows:
        install_root = bin_dir.parent    # MA_HOME — e.g. %LOCALAPPDATA%\MemoryArchive
        try:
            if install_root.exists() and not any(install_root.rglob("*")):
                shutil.rmtree(install_root, ignore_errors=True)
                _console.print(
                    f"[green]  [OK] Removed directory: {install_root}[/green]"
                )
        except Exception:
            pass

        # Remove INSTALL_DIR (MA_HOME\bin) from the user's persistent PATH registry entry.
        try:
            import winreg
            key = winreg.OpenKey(
                winreg.HKEY_CURRENT_USER, "Environment", 0, winreg.KEY_ALL_ACCESS
            )
            try:
                raw_path, _ = winreg.QueryValueEx(key, "Path")
            except FileNotFoundError:
                raw_path = ""
            target    = str(bin_dir).lower()
            new_parts = [
                part for part in raw_path.split(";")
                if part.strip() and part.strip().lower() != target
            ]
            if len(new_parts) < len([p for p in raw_path.split(";") if p.strip()]):
                winreg.SetValueEx(
                    key, "Path", 0, winreg.REG_EXPAND_SZ, ";".join(new_parts)
                )
                _console.print(
                    "[green]  [OK] Removed entry from Windows PATH[/green]"
                )
            winreg.CloseKey(key)
        except ImportError:
            pass
        except Exception as exc:
            _console.print(f"[yellow]  [!!] Could not update PATH: {exc}[/yellow]")

    else:
        # Linux / macOS: remove the PATH export the installer wrote to rc files.
        _remove_path_from_shell_rc(bin_dir)

        # Remove the now-empty bin directory (binaries + launcher gone).
        try:
            if bin_dir.exists() and not any(bin_dir.iterdir()):
                bin_dir.rmdir()
        except OSError:
            pass

        # Remove MA_HOME if it is now empty.
        ma_home = bin_dir.parent        # ~/.memory-archive or /opt/memory-archive
        try:
            if ma_home.exists() and not any(ma_home.rglob("*")):
                shutil.rmtree(ma_home, ignore_errors=True)
                _console.print(
                    f"[green]  [OK] Removed directory: {ma_home}[/green]"
                )
        except Exception:
            pass

    # 11. Result summary
    _console.print("")
    _console.print("=" * 60)

    if removed and not failed:
        _console.print("[green bold]  Uninstall completed successfully![/green bold]")
        _console.print(f"\n  Removed {len(removed)} item(s).")
    elif removed and failed:
        _console.print("[yellow bold]  Uninstall partially completed.[/yellow bold]")
        _console.print(f"\n  Removed: {len(removed)}    Failed: {len(failed)}")
        for path, reason in failed:
            _console.print(f"  - {path}: {reason}")
        if not is_windows:
            _console.print("\n  Tip: re-run with sudo for system-wide paths.")
    else:
        _console.print("[red bold]  Uninstall failed.[/red bold]")
        for path, reason in failed:
            _console.print(f"  - {path}: {reason}")
        raise typer.Exit(code=1)

    _console.print("")
    _console.print("  Thank you for using Memory Archive!")
    _console.print(f"  Feedback: {REPO_URL}/issues")
    _console.print("")


# =============================================================================
# Registration helper — the only public symbol cli.py needs to import
# =============================================================================

def register_commands(app: typer.Typer) -> None:
    """
    Attach `update` and `uninstall` to the main Typer app.

    Call this at the bottom of cli.py, just before `if __name__ == "__main__":`:

        from ma_app.updater import register_commands
        register_commands(app)
    """
    app.command(name="update")(_update_command)
    app.command(name="uninstall")(_uninstall_command)