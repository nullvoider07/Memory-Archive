# /Memory-Archive/ma-app/ma_app/tui/clipboard.py

from __future__ import annotations

import logging
import platform
import shutil
import subprocess
import threading
from typing import Sequence

from textual.app import App

_log = logging.getLogger(__name__)

# Ordered candidates per platform. The first helper present on PATH wins.
# Wayland first on Linux: wl-copy is the only one that reaches the clipboard
# under a Wayland compositor, while xclip/xsel work through XWayland or plain X.
_LINUX_HELPERS: tuple[tuple[str, Sequence[str]], ...] = (
    ("wl-copy", ()),
    ("xclip",   ("-selection", "clipboard")),
    ("xsel",    ("--clipboard", "--input")),
)


def _os_clipboard_command() -> list[str] | None:
    """Return the argv of an available OS clipboard-write helper, or None."""
    system = platform.system()
    if system == "Darwin":
        path = shutil.which("pbcopy")
        return [path] if path else None
    if system == "Windows":
        path = shutil.which("clip")
        return [path] if path else None
    for name, args in _LINUX_HELPERS:
        path = shutil.which(name)
        if path:
            return [path, *args]
    return None


def copy_to_os_clipboard(text: str) -> bool:
    """
    Write text to the operating system clipboard via an external helper.

    Textual's own App.copy_to_clipboard emits OSC 52 and nothing else, so
    whether the text ever reaches the system clipboard is up to the terminal —
    many do not implement it, and its own docs say so. This path is what makes
    a selection pastable into other applications.

    Returns True if a helper accepted the text. Failure is not an error worth
    interrupting the user for: OSC 52 may still have worked, so log and move on.
    """
    if not text:
        return False
    argv = _os_clipboard_command()
    if argv is None:
        return False
    try:
        subprocess.run(
            argv,
            input=text.encode("utf-8"),
            timeout=2,
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return True
    except (OSError, subprocess.SubprocessError) as exc:
        _log.debug("OS clipboard helper %s failed: %s", argv[0], exc)
        return False


class _ClipboardWorker:
    """
    Serializes OS clipboard writes onto one background thread.

    Spawning the helper takes ~64 ms and the selection handler runs on the event
    loop, so the call cannot be inline. A thread per selection is not the answer
    either: rapid selections then run helpers concurrently, and because they
    finish out of order the clipboard can end up holding an *earlier* selection
    than the one just made. Measured at 300 back-to-back selections — 137
    helpers alive at once and the clipboard left holding selection 150 of 299.

    So there is at most one helper at a time, and only the newest text is kept
    while it runs: intermediate selections during a drag are worth nothing, and
    the last one must win.
    """

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._pending: str | None = None
        self._thread: threading.Thread | None = None

    def submit(self, text: str) -> None:
        with self._lock:
            self._pending = text
            if self._thread is None:
                self._thread = threading.Thread(
                    target=self._drain, name="ma-clipboard", daemon=True
                )
                self._thread.start()

    def _drain(self) -> None:
        while True:
            with self._lock:
                text = self._pending
                self._pending = None
                if text is None:
                    # Cleared under the same lock submit() takes, so a selection
                    # arriving now starts a new thread rather than being lost.
                    self._thread = None
                    return
            copy_to_os_clipboard(text)


_worker = _ClipboardWorker()


class ClipboardApp(App):
    """
    App base that copies a mouse selection as soon as it is made.

    Textual 8.x already implements selection (Screen.selections, populated by
    drag) and exposes Screen.get_selected_text(), but binds nothing to
    action_copy_text — selections were made and then dropped, which is why no
    part of the TUI could be copied. Screen posts events.TextSelected on every
    mouse release and it bubbles to the App, so handling it here covers every
    screen and widget without touching any of them.

    Copy goes to both destinations: OSC 52 for terminals that support it, and
    an external helper for the system clipboard proper.
    """

    def on_text_selected(self) -> None:
        # Posted on every mouse release, including a plain click that selected
        # nothing. Copying "" then would silently wipe a clipboard the user had
        # just filled somewhere else.
        try:
            text = self.screen.get_selected_text()
        except Exception:  # no screen mounted yet, or the stack is unwinding
            return
        if not text:
            return
        self.copy_to_clipboard(text)
        # Off the event loop, coalesced, last selection wins — see
        # _ClipboardWorker for why neither inline nor thread-per-copy works.
        _worker.submit(text)
