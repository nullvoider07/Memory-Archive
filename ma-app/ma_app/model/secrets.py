# /Memory-Archive/ma-app/ma_app/model/secrets.py

from __future__ import annotations

import logging
import os
import re
from pathlib import Path

_log = logging.getLogger(__name__)

_ENV_VAR_RE = re.compile(r'^[A-Z][A-Z0-9_]{0,253}$')


def resolve_api_key(ref: str) -> str:
    """
    Resolve a model_api_key_ref reference string to the actual API key value.

    Supported reference formats:
        env:VAR_NAME        — read from environment variable VAR_NAME
        file:/abs/path      — read file contents (Docker secrets, k8s volume mounts)
        (empty string)      — no authentication; returns empty string

    Any other value is treated as a literal key and a warning is logged.
    Literal keys in config are strongly discouraged — use env: or file: in production.

    Security properties:
    - VAR_NAME is validated against [A-Z][A-Z0-9_]* to block injection attacks
    - File paths must be absolute to prevent relative path traversal
    - The resolved key value is never logged anywhere in this function
    - Raises ValueError for malformed refs so the session fails fast at construction
      time rather than sending unauthenticated requests silently

    Args:
        ref: the model_api_key_ref string from session_config or settings.model.api_key_ref

    Returns:
        The plaintext API key, or empty string if ref is empty.

    Raises:
        ValueError: if the ref format is invalid, the env var is not set,
                    or the file cannot be read.
    """
    if not ref:
        return ""

    if ref.startswith("env:"):
        var_name = ref[4:]
        if not _ENV_VAR_RE.match(var_name):
            raise ValueError(
                f"Invalid environment variable name in model_api_key_ref: {var_name!r}. "
                "Only names matching [A-Z][A-Z0-9_]* are permitted."
            )
        value = os.environ.get(var_name)
        if value is None:
            raise ValueError(
                f"Environment variable {var_name!r} is referenced in model_api_key_ref "
                "but is not set in the current environment. "
                "Set it before starting ma-app."
            )
        if not value.strip():
            raise ValueError(
                f"Environment variable {var_name!r} is set but empty. "
                "The API key must be a non-empty string."
            )
        return value

    if ref.startswith("file:"):
        path_str = ref[5:]
        path = Path(path_str)
        if not path.is_absolute():
            raise ValueError(
                f"File path in model_api_key_ref must be absolute, got: {path_str!r}. "
                "Use an absolute path like 'file:/run/secrets/vlm_api_key'."
            )
        try:
            value = path.read_text(encoding="utf-8").strip()
        except OSError as e:
            raise ValueError(
                f"Failed to read API key from {path_str!r}: {e}"
            ) from e
        if not value:
            raise ValueError(
                f"File {path_str!r} is empty. The API key must be a non-empty string."
            )
        return value

    _log.warning(
        "model_api_key_ref does not use the 'env:' or 'file:' prefix. "
        "Treating the value as a literal API key. "
        "This is not recommended for production — "
        "use 'env:VAR_NAME' or 'file:/path/to/secret' instead."
    )
    return ref