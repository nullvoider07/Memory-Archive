# /Memory-Archive/ma-app/ma_app/tui/app.py

from __future__ import annotations
from pathlib import Path
from textual.app import App
from typing import ClassVar
from textual.binding import Binding, BindingType
from ma_app.tui.screens.annotation import AnnotationScreen
from ma_app.tui.screens.compiler import CompilerScreen
from ma_app.tui.session_loader import SessionState


class AnnotationApp(App):
    """
    Memory Archive TUI — root Textual application.

    Owns the screen stack. On startup, pushes AnnotationScreen as the
    sole screen. The help overlay is pushed/popped on top of it.
    """

    CSS_PATH = None

    def __init__(self, session_data: SessionState) -> None:
        super().__init__()
        self._session_data = session_data
        self._session_explicitly_closed = False
        self._heartbeat_timer = None

    def on_mount(self) -> None:
        self.push_screen(AnnotationScreen(self._session_data))
        if self._session_data.claim_id:
            self._heartbeat_timer = self.set_interval(5 * 60, self._send_heartbeat)

    def _send_heartbeat(self) -> None:
        """Refresh the claim TTL every 5 minutes while the TUI is open."""
        import threading
        from ma_app.ipc.client import IPCClient, IPCError
        import logging

        claim_id = self._session_data.claim_id
        session_id = self._session_data.session_id

        def _run() -> None:
            try:
                with IPCClient() as client:
                    response = client.send({
                        "type": "heartbeat_claim",
                        "session_id": session_id,
                        "claim_id": claim_id,
                    })
                if response.get("type") == "error" and response.get("code") == "CLAIM_LOST":
                    logging.getLogger(__name__).warning(
                        "Claim on session %s has expired — your work is still saved locally "
                        "but the session may be claimed by another annotator.",
                        session_id,
                    )
            except IPCError:
                pass

        threading.Thread(target=_run, daemon=True).start()

    def mark_annotation_complete(self) -> None:
        """Called by AnnotationScreen when the user finishes all steps."""
        self._annotation_complete = True

    def mark_session_closed(self) -> None:
        """
        Called by AnnotationScreen._quit_cleanly() after close_session() IPC fires.
        Prevents on_unmount from issuing a duplicate close_session() call.
        """
        self._session_explicitly_closed = True

    def on_unmount(self) -> None:
        if getattr(self, "_annotation_complete", False):
            self._cleanup_temp_dir()
            return
        if not self._session_explicitly_closed:
            from ma_app.tui.reasoning_writer import ReasoningWriter
            writer = ReasoningWriter(
                self._session_data.memory_dir,
                self._session_data.session_id,
            )
            writer.close_session()
        self._cleanup_temp_dir()

    def _cleanup_temp_dir(self) -> None:
        import shutil
        import logging
        temp_dir = getattr(self._session_data, "temp_dir", None)
        if temp_dir is not None and temp_dir.exists():
            shutil.rmtree(temp_dir, ignore_errors=True)
            logging.getLogger(__name__).debug("Deleted temp session dir: %s", temp_dir)


class CompilerApp(App):
    """
    Memory Archive compiler TUI — wraps the memory.md editor screen.

    Pushes CompilerScreen on mount. Exits with result='complete' when
    the user saves and closes the editor.
    """

    CSS_PATH = None
    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("ctrl+q", "quit_editor", "Quit", show=False),
        Binding("ctrl+s", "save", "Save", show=False),
    ]

    def action_quit_editor(self) -> None:
        try:
            screen = self.screen
            if hasattr(screen, "action_quit_editor"):
                screen.action_quit_editor()
            else:
                self.exit(result="complete")
        except Exception:
            self.exit(result="complete")

    def action_save(self) -> None:
        try:
            screen = self.screen
            if hasattr(screen, "action_save"):
                screen.action_save()
        except Exception:
            pass

    def __init__(self, session_id: str, memory_path: Path) -> None:
        super().__init__()
        self._session_id  = session_id
        self._memory_path = memory_path

    def on_mount(self) -> None:
        try:
            text = self._memory_path.read_text(encoding="utf-8")
        except OSError:
            text = ""
        self.push_screen(CompilerScreen(self._memory_path, text, self._session_id))