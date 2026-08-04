"""Tests for replacing a binary that may currently be running.

`memory-archive update` copied the new binary over the old path in place, which
Linux refuses with ETXTBSY when the target is a running executable — and ma-core
normally is. The fix writes a sibling temp file and renames it over the target:
rename(2) is permitted, and the running process keeps its old inode.
"""

from __future__ import annotations

import errno
import os
import shutil
import stat
import subprocess
import tempfile
import time
from pathlib import Path

import pytest

from ma_app.updater import _replace_binary_posix


pytestmark = pytest.mark.skipif(
    os.name == "nt", reason="POSIX binary replacement path"
)


def _write_binary(path: Path, body: str) -> Path:
    path.write_text(f"#!/bin/sh\n{body}\n", encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


def test_the_destination_gets_the_new_contents(tmp_path):
    src = _write_binary(tmp_path / "src", "echo new")
    dst = _write_binary(tmp_path / "bin", "echo old")

    _replace_binary_posix(src, dst, is_macos=False)

    assert "echo new" in dst.read_text(encoding="utf-8")


def test_replacement_leaves_no_temp_file_behind(tmp_path):
    src = _write_binary(tmp_path / "src", "echo new")
    dst = _write_binary(tmp_path / "bin", "echo old")

    _replace_binary_posix(src, dst, is_macos=False)

    leftovers = [p.name for p in tmp_path.iterdir() if p.name.startswith("bin.new-")]
    assert leftovers == []


def test_the_replaced_file_is_a_new_inode(tmp_path):
    """
    The whole point: the old inode survives for whoever still has it open.
    Overwriting in place would have kept the inode and hit ETXTBSY.
    """
    src = _write_binary(tmp_path / "src", "echo new")
    dst = _write_binary(tmp_path / "bin", "echo old")
    old_inode = dst.stat().st_ino

    _replace_binary_posix(src, dst, is_macos=False)

    assert dst.stat().st_ino != old_inode


def test_it_replaces_a_binary_that_is_currently_running(tmp_path):
    """
    The reported failure, reproduced: a running executable is replaced while it
    runs. In-place copy raises ETXTBSY here; rename does not.

    This needs a real ELF, not a shell script — a script is executed by the
    interpreter, so the kernel never holds the script file as a running text
    image and writing to it is allowed.
    """
    real_binary = shutil.which("sleep")
    if real_binary is None:
        pytest.skip("no `sleep` binary available to stand in for ma-core")

    # Keep the name: coreutils ships as a multi-call binary that dispatches on
    # argv[0], so a copy named anything else exits immediately with
    # "unknown program". A dead child holds no text image and ETXTBSY never
    # fires, which would make this test pass or fail on timing alone.
    dst = tmp_path / "sleep"
    shutil.copy2(real_binary, dst)
    src = _write_binary(tmp_path / "src", "echo new")

    proc = subprocess.Popen(
        [str(dst), "30"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    try:
        time.sleep(0.3)
        assert proc.poll() is None, "the stand-in binary exited before the test ran"
        # Confirm the hazard is real on this platform before claiming a fix.
        with pytest.raises(OSError) as excinfo:
            with open(dst, "wb") as handle:
                handle.write(b"x")
        assert excinfo.value.errno == errno.ETXTBSY

        _replace_binary_posix(src, dst, is_macos=False)
        assert "echo new" in dst.read_text(encoding="utf-8")
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def test_the_new_binary_is_executable(tmp_path):
    """
    chmod happens on the temp file before the rename, so the binary is never
    visible at its final path without the exec bit.
    """
    src = tmp_path / "src"
    src.write_text("#!/bin/sh\necho new\n", encoding="utf-8")  # not executable
    dst = _write_binary(tmp_path / "bin", "echo old")

    _replace_binary_posix(src, dst, is_macos=False)

    assert dst.stat().st_mode & stat.S_IXUSR


def test_the_temp_file_name_is_not_predictable(tmp_path):
    """
    A predictable temp name in a directory another user can write to lets them
    pre-plant a symlink there and redirect the copy. mkstemp's O_EXCL plus a
    random suffix is what closes that; assert the name is not derivable.
    """
    src = _write_binary(tmp_path / "src", "echo new")
    dst = _write_binary(tmp_path / "bin", "echo old")

    seen: list[str] = []
    real_mkstemp = tempfile.mkstemp

    def spy(*args, **kwargs):
        fd, name = real_mkstemp(*args, **kwargs)
        seen.append(Path(name).name)
        return fd, name

    import ma_app.updater as updater_module

    original = updater_module.tempfile.mkstemp
    updater_module.tempfile.mkstemp = spy  # type: ignore[assignment]
    try:
        _replace_binary_posix(src, dst, is_macos=False)
    finally:
        updater_module.tempfile.mkstemp = original  # type: ignore[assignment]

    assert seen, "the replacement must go through a temp file"
    assert seen[0] != f"bin.new-{os.getpid()}", "temp name must not be the PID"
    assert seen[0].startswith("bin.new-")
    assert len(seen[0]) > len("bin.new-"), "expected a random suffix"


def test_a_symlinked_temp_path_cannot_redirect_the_write(tmp_path):
    """
    The attack the O_EXCL open prevents: a symlink sitting where the temp file
    is about to be created must not become the write target.
    """
    outside = tmp_path / "outside.txt"
    outside.write_text("must not be overwritten", encoding="utf-8")

    src = _write_binary(tmp_path / "src", "echo new")
    dst = _write_binary(tmp_path / "bin", "echo old")

    # Plant the symlink under the name the pre-hardening code would have used.
    planted = tmp_path / f"bin.new-{os.getpid()}"
    planted.symlink_to(outside)

    _replace_binary_posix(src, dst, is_macos=False)

    assert outside.read_text(encoding="utf-8") == "must not be overwritten"
    assert "echo new" in dst.read_text(encoding="utf-8")


def test_a_failed_copy_does_not_touch_the_destination(tmp_path):
    src = tmp_path / "missing"
    dst = _write_binary(tmp_path / "bin", "echo old")

    with pytest.raises(OSError):
        _replace_binary_posix(src, dst, is_macos=False)

    assert "echo old" in dst.read_text(encoding="utf-8")
    leftovers = [p.name for p in tmp_path.iterdir() if p.name.startswith("bin.new-")]
    assert leftovers == [], "the temp file must be cleaned up on failure"
