# /Memory-Archive/ma-app/ma_app/tui/widgets/image_review.py

from __future__ import annotations

import shutil
import subprocess
import threading
from pathlib import Path
import platform as _platform
from typing import TYPE_CHECKING, ClassVar, Optional

from textual.app import ComposeResult
from textual.binding import Binding, BindingType
from textual.css.query import NoMatches
from textual.widget import Widget
from textual.widgets import Button, Label, Static

from ma_app.tui.session_loader import StepState

if TYPE_CHECKING:
    from ma_app.storage.remote_fetch import RemoteFetcher

_HAS_FEH     = shutil.which("feh") is not None
_IS_MACOS    = _platform.system() == "Darwin"
_IS_WINDOWS  = _platform.system() == "Windows"
_HAS_OPEN    = _IS_MACOS and shutil.which("open") is not None


def _read_image_size(path: Path) -> tuple[int, int] | None:
    try:
        from PIL import Image as _Image
        with _Image.open(path) as img:
            return img.size
    except Exception:
        pass
    try:
        with open(path, "rb") as f:
            header = f.read(24)
        if len(header) >= 24 and header[:8] == b"\x89PNG\r\n\x1a\n":
            import struct as _s
            w = _s.unpack(">I", header[16:20])[0]
            h = _s.unpack(">I", header[20:24])[0]
            return w, h
    except OSError:
        pass
    return None


class ImageReview(Widget, can_focus=True):
    """
    Image preview pane.

    Shows image metadata and an 'Open' button.
    Enter or click opens the image fullscreen in feh (Linux) or open (macOS).

    In remote annotation mode, images that are not yet cached locally are
    fetched on-demand via the RemoteFetcher proxy. A background thread handles
    the fetch and reloads the display when the download completes.

    Public API: load_step(step), prefetch_step(step)
    """

    BINDINGS: ClassVar[list[BindingType]] = [
        Binding("enter", "open_image", "Open image", show=False),
    ]

    DEFAULT_CSS = """
    ImageReview {
        width: 1fr;
        height: 1fr;
        layout: vertical;
        border: round #2a2d3a;
        background: #0f1117;
        align: center middle;
    }

    ImageReview > #img-title-bar {
        width: 1fr;
        height: 1;
        background: #1a1d27;
        color: #e2e8f0;
        padding: 0 1;
        dock: top;
    }

    ImageReview > #img-card {
        width: 42;
        height: auto;
        background: #1a1d27;
        border: round #3a3d4a;
        padding: 2 3;
        align: center middle;
        layout: vertical;
    }

    ImageReview > #img-card > #img-icon {
        width: 1fr;
        height: 3;
        color: #5865f2;
        text-align: center;
        content-align: center middle;
    }

    ImageReview > #img-card > #img-filename {
        width: 1fr;
        height: 1;
        color: #e2e8f0;
        text-align: center;
        content-align: center middle;
        margin: 1 0 0 0;
    }

    ImageReview > #img-card > #img-dims {
        width: 1fr;
        height: 1;
        color: #64748b;
        text-align: center;
        content-align: center middle;
    }

    ImageReview > #img-card > #img-type {
        width: 1fr;
        height: 1;
        color: #64748b;
        text-align: center;
        content-align: center middle;
        margin: 0 0 2 0;
    }

    ImageReview > #img-card > #open-btn {
        width: 1fr;
        height: 3;
        background: #0f1117;
        color: #e2e8f0;
        border: round #64748b;
        text-align: center;
        content-align: center middle;
    }

    ImageReview > #img-card > #open-btn:hover {
        background: #1a1d27;
        border: round #e2e8f0;
        color: #ffffff;
    }

    ImageReview > #img-card > #open-btn:focus {
        background: #1a1d27;
        border: round #5865f2;
        color: #ffffff;
    }

    ImageReview > #no-step-label {
        color: #3a3d4a;
        text-align: center;
    }
    """

    def __init__(
        self,
        memory_dir: Path,
        remote_fetcher: Optional[RemoteFetcher] = None,
    ) -> None:
        super().__init__()
        self._memory_dir = memory_dir
        self._remote_fetcher = remote_fetcher
        self._step: Optional[StepState] = None
        self._path: Optional[Path] = None
        self._frame_paths: list[Path] = []
        self._fetching: bool = False

    def compose(self) -> ComposeResult:
        yield Label("IMAGE", id="img-title-bar")

        with Static(id="img-card"):
            yield Label("", id="img-icon")
            yield Label("", id="img-filename")
            yield Label("", id="img-dims")
            yield Label("", id="img-type")
            yield Button("Open fullscreen", id="open-btn")

        yield Label("No step selected.", id="no-step-label")

    def on_mount(self) -> None:
        self._update_display()

    def load_step(self, step: StepState) -> None:
        """
        Load a step into the pane without stealing focus.

        Resolves image paths from local disk (or remote cache if in remote
        mode). If the at-frame is not yet available, triggers a background
        fetch and refreshes the display when the download completes.
        """
        self._step = step
        self._path = None
        self._frame_paths = []

        for rel in (step.before_image_path, step.image_path, step.after_image_path):
            if rel:
                p = self._resolve_path(rel)
                if p is not None and p.exists():
                    self._frame_paths.append(p)
                    if rel == step.image_path:
                        self._path = p

        if self._remote_fetcher is not None and self._path is None and step.image_path:
            self._start_background_fetch(step)

        self._update_display()

    def prefetch_step(self, step: StepState) -> None:
        """
        Trigger a background pre-fetch of a step's images into the remote cache.

        Called when the annotator saves the current step so the next step's
        images are ready before they navigate there. No-op in local mode.
        """
        if self._remote_fetcher is None:
            return
        image_paths = [
            p for p in [step.before_image_path, step.image_path, step.after_image_path]
            if p
        ]
        if image_paths:
            self._remote_fetcher.prefetch_step_images(image_paths)

    def _resolve_path(self, rel: str) -> Optional[Path]:
        """
        Resolve a relative image path to a local Path.

        In remote mode, checks the image cache directory first. Falls back
        to memory_dir (covers local mode and cloud_primary temp dir reads).
        """
        if self._remote_fetcher is not None:
            cached = self._remote_fetcher.img_cache_dir / rel
            if cached.exists():
                return cached
        return self._memory_dir / rel

    def _start_background_fetch(self, step: StepState) -> None:
        if self._fetching:
            return
        self._fetching = True
        threading.Thread(
            target=self._fetch_step_and_reload,
            args=(step,),
            daemon=True,
        ).start()

    def _fetch_step_and_reload(self, step: StepState) -> None:
        fetcher = self._remote_fetcher
        if fetcher is None:
            self._fetching = False
            return
        for rel in [step.before_image_path, step.image_path, step.after_image_path]:
            if rel:
                fetcher.fetch_and_cache_image(rel)
        self._fetching = False
        if self._step is step:
            self.call_from_thread(self._reload_after_fetch, step)

    def _reload_after_fetch(self, step: StepState) -> None:
        if self._step is step:
            self.load_step(step)

    def _update_display(self) -> None:
        has_image = self._path is not None

        try:
            card     = self.query_one("#img-card",      Static)
            no_label = self.query_one("#no-step-label", Label)

            if self._step is None:
                card.display     = False
                no_label.display = True
                self._set_label("img-title-bar", "IMAGE")
                return

            step = self._step  # narrowed: not None past this point

            card.display     = True
            no_label.display = False

            if not step.image_path:
                self._set_label("img-title-bar", "IMAGE  ·  no image captured")
            elif not has_image:
                if self._fetching:
                    self._set_label("img-title-bar", "IMAGE  ·  fetching…")
                else:
                    self._set_label("img-title-bar",
                        f"IMAGE  ·  {Path(step.image_path).name}  ·  NOT FOUND")
            else:
                mark_str = "marked" if step.marked else "unmarked"
                self._set_label("img-title-bar",
                    f"IMAGE  ·  {Path(step.image_path).name}  ·  {mark_str}")

            if has_image and self._path is not None:
                icon = "◉" if step.marked else "○"
                self._set_label("img-icon", icon)

                name = self._path.name
                if len(name) > 34:
                    name = name[:15] + "…" + name[-16:]
                self._set_label("img-filename", name)

                size = _read_image_size(self._path)
                dim_str = f"{size[0]} × {size[1]} px" if size else "unknown size"
                self._set_label("img-dims", dim_str)

                action = f"{step.action_type} / {step.action_subtype}"
                self._set_label("img-type", action)

                btn = self.query_one("#open-btn", Button)
                _can_open = _HAS_FEH or _HAS_OPEN
                btn.disabled = not _can_open
                if not _can_open:
                    btn.label = "no image viewer available"
            else:
                self._set_label("img-icon", "↓" if self._fetching else "✗")
                self._set_label("img-filename", "Image not found" + (" (fetching…)" if self._fetching else ""))
                self._set_label("img-dims", "")
                self._set_label("img-type", "")
                btn = self.query_one("#open-btn", Button)
                btn.disabled = True
                btn.label = "No image available"

        except NoMatches:
            pass

    def _set_label(self, widget_id: str, text: str) -> None:
        try:
            self.query_one(f"#{widget_id}", Label).update(text)
        except NoMatches:
            pass

    def action_open_image(self) -> None:
        self._open_image()

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "open-btn":
            event.stop()
            self._open_image()

    def _open_image(self) -> None:
        if not self._frame_paths:
            return
        target = self._path or self._frame_paths[0]
        if _HAS_FEH:
            cmd = ["feh", "--fullscreen", "--auto-zoom"]
            cmd += [str(p.resolve()) for p in self._frame_paths]
            if self._path:
                cmd += ["--start-at", str(self._path.resolve())]
            subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        elif _HAS_OPEN:
            subprocess.Popen(
                ["open", str(target.resolve())],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        elif _IS_WINDOWS:
            import os as _os
            _os.startfile(str(target.resolve()))