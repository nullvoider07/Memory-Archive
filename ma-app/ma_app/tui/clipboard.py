# /Memory-Archive/ma-app/ma_app/tui/clipboard.py

from __future__ import annotations

import logging
import platform
import shutil
import subprocess
import threading
from typing import Sequence

from textual import events
from textual.app import App, ScreenStackError
from textual.errors import NoWidget
from textual.widget import Widget

_log = logging.getLogger(__name__)

# Ordered candidates per platform. The first helper present on PATH wins.
# Wayland first on Linux: wl-copy is the only one that reaches the clipboard
# under a Wayland compositor, while xclip/xsel work through XWayland or plain X.
_LINUX_HELPERS: tuple[tuple[str, Sequence[str]], ...] = (
    ("wl-copy", ()),
    ("xclip",   ("-selection", "clipboard")),
    ("xsel",    ("--clipboard", "--input")),
)

_LINUX_PASTE_HELPERS: tuple[tuple[str, Sequence[str]], ...] = (
    ("wl-paste", ("--no-newline",)),
    ("xclip",    ("-selection", "clipboard", "-o")),
    ("xsel",     ("--clipboard", "--output")),
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


def _os_paste_command() -> list[str] | None:
    """Return the argv of an available OS clipboard-read helper, or None."""
    system = platform.system()
    if system == "Darwin":
        path = shutil.which("pbpaste")
        return [path] if path else None
    if system == "Windows":
        path = shutil.which("powershell")
        return [path, "-NoProfile", "-Command", "Get-Clipboard"] if path else None
    for name, args in _LINUX_PASTE_HELPERS:
        path = shutil.which(name)
        if path:
            return [path, *args]
    return None


def paste_from_os_clipboard() -> str:
    """
    Read the operating system clipboard via an external helper.

    Returns an empty string when no helper is available or the read fails —
    a paste that yields nothing is preferable to an error dialog mid-edit.
    """
    argv = _os_paste_command()
    if argv is None:
        return ""
    try:
        result = subprocess.run(
            argv,
            capture_output=True,
            timeout=2,
            check=False,
            stdin=subprocess.DEVNULL,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        _log.debug("OS clipboard read helper %s failed: %s", argv[0], exc)
        return ""
    if result.returncode != 0:
        return ""
    return result.stdout.decode("utf-8", errors="replace")


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
    App base that copies a mouse selection as soon as it is made, anywhere.

    A terminal application only sees the mouse at all because Textual asks for
    it: the driver writes SET_ANY_EVENT_MOUSE and friends on startup
    (textual/drivers/linux_driver.py). That takes the mouse away from the
    terminal emulator, so the terminal's own drag-to-select — the mechanism
    every other CLI relies on for copying — no longer applies here. Selection
    has to be implemented in-app, and two separate models implement it:

    1.  **Screen-level selection.** Textual populates Screen.selections on drag
        and exposes Screen.get_selected_text(), but binds nothing to
        action_copy_text, so selections were made and then dropped. This covers
        static content: labels, step rows, stats, help text.

    2.  **Widget-owned selection.** TextArea and Input call capture_mouse() on
        mouse-down (textual/widgets/_text_area.py, _input.py), and
        Screen._forward_event only opens a screen-level selection when
        `not self.app.mouse_captured`. So inside the compile editor, the
        reasoning editor and the jump box — every editable region — the screen
        records nothing and get_selected_text() returns None. The text is
        selected, in that widget's own `selected_text`. Covering only model 1
        is why copying appeared to work everywhere except where it mattered.

    So the selection is read from the screen first and from the widget the drag
    began on second. The origin widget is recorded on mouse-down because the
    capture is already released by the time TextSelected is handled, and a drag
    may end over a different widget than it started on.

    Copy goes to both destinations: OSC 52 for terminals that support it, and
    an external helper for the system clipboard proper. Routing it through the
    copy_to_clipboard override below also picks up every copy Textual performs
    internally — Ctrl+C in a TextArea or Input, and cut — which reach the OS
    clipboard for the same reason.
    """

    def __init__(self, *args, **kwargs) -> None:
        super().__init__(*args, **kwargs)
        # Widget the current drag started on; the only reliable handle on a
        # widget-owned selection once the mouse capture has been released.
        self._selection_origin: Widget | None = None

    def copy_to_clipboard(self, text: str) -> None:
        """
        Copy to the terminal (OSC 52) and to the OS clipboard.

        Overriding here rather than at each call site is deliberate: every
        Textual copy path — TextArea.action_copy, Input.action_copy, both cut
        actions, Screen.action_copy_text — funnels through App.copy_to_clipboard,
        so one override makes all of them reach the system clipboard.
        """
        super().copy_to_clipboard(text)
        if text:
            # Off the event loop, coalesced, last selection wins — see
            # _ClipboardWorker for why neither inline nor thread-per-copy works.
            _worker.submit(text)

    def on_mouse_down(self, event: events.MouseDown) -> None:
        try:
            widget, _ = self.get_widget_at(*event.screen_offset)
        except (NoWidget, ScreenStackError):
            widget = None
        self._selection_origin = widget

    def on_text_selected(self) -> None:
        # Posted on every mouse release, including a plain click that selected
        # nothing. Copying "" then would silently wipe a clipboard the user had
        # just filled somewhere else.
        text = self.current_selection()
        if text:
            self.copy_to_clipboard(text)

    def current_selection(self) -> str:
        """Return the selected text from whichever selection model holds it."""
        try:
            screen_text = self.screen.get_selected_text()
        except ScreenStackError:  # no screen mounted yet, or the stack is unwinding
            screen_text = None
        if screen_text:
            return screen_text

        origin = self._selection_origin
        if origin is None:
            return ""
        try:
            # TextArea and Input both expose the property; anything else that
            # captures the mouse (scrollbars) does not, and selects nothing.
            # It is a computed property reading the widget's document, and the
            # origin is held from mouse-down to mouse-up — long enough for the
            # widget to have been removed from the DOM under it. A copy is
            # never worth taking the app down for.
            widget_text = getattr(origin, "selected_text", None)
        except Exception:
            _log.debug("origin widget %r could not report a selection", origin)
            return ""
        return widget_text if isinstance(widget_text, str) else ""
