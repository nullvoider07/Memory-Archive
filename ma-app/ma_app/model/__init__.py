# /Memory-Archive/ma-app/ma_app/model/__init__.py

from __future__ import annotations

from ma_app.model.backend import (
    ContextStep,
    ModelBackend,
    ModelError,
    NonRetryableError,
    ReasoningResult,
    RetryableError,
    StepData,
)
from ma_app.model.generic import GenericApiModelBackend
from ma_app.model.internal_model import InternalModelBackend
from ma_app.model.pipeline import ReasoningPipeline
from ma_app.model.router import ModelRouter

__all__ = [
    "ContextStep",
    "GenericApiModelBackend",
    "InternalModelBackend",
    "ModelBackend",
    "ModelError",
    "ModelRouter",
    "NonRetryableError",
    "ReasoningPipeline",
    "ReasoningResult",
    "RetryableError",
    "StepData",
    "get_backend",
    "get_router",
]


def get_router(settings=None) -> ModelRouter:
    """
    Build a ModelRouter from settings.

    Flat config (model.provider + model.endpoint + model.api_key_ref):
        Creates a single-entry pool with 'pinned' policy. Fully backward compatible.

    Pool config (model.providers array):
        Creates a multi-entry pool with the configured routing_policy.
        Each provider entry instantiates its own backend with its own api_key_ref.

    Raises ValueError if any provider config is invalid or key resolution fails.
    """
    if settings is None:
        from ma_app.config.settings import Settings
        settings = Settings.load()

    from ma_app.model.router import _BackendEntry

    if settings.model.providers:
        policy = settings.model.routing_policy or "pinned"
        entries: list[_BackendEntry] = []
        for p in settings.model.providers:
            if not p.name:
                raise ValueError("Each provider in model.providers must have a 'name' field.")
            if not p.endpoint:
                raise ValueError(
                    f"Provider '{p.name}' in model.providers is missing 'endpoint'."
                )
            backend = _instantiate_backend(
                name=p.name,
                endpoint=p.endpoint,
                api_key_ref=p.api_key_ref,
                timeout_seconds=settings.model.request_timeout_seconds,
            )
            entries.append(_BackendEntry(name=p.name, backend=backend, priority=p.priority))
        return ModelRouter(entries=entries, policy=policy)

    # Flat config → single-entry pinned router
    backend = get_backend(settings)
    provider_name = settings.model.provider or "default"
    entries = [_BackendEntry(name=provider_name, backend=backend, priority=1)]
    return ModelRouter(entries=entries, policy="pinned")


def _instantiate_backend(
    name: str,
    endpoint: str,
    api_key_ref: str,
    timeout_seconds: int,
) -> ModelBackend:
    """Select and construct the right backend implementation for a named provider."""
    from ma_app.model.internal_model import InternalModelBackend
    from ma_app.model.generic import GenericApiModelBackend

    name_lower = name.lower()
    if "xai" in name_lower or "internal" in name_lower:
        return InternalModelBackend(
            endpoint=endpoint,
            api_key_ref=api_key_ref,
            timeout_seconds=timeout_seconds,
        )
    return GenericApiModelBackend(
        endpoint=endpoint,
        api_key_ref=api_key_ref,
        timeout_seconds=timeout_seconds,
    )


def get_backend(settings=None) -> ModelBackend:
    """
    Instantiate and return the configured ModelBackend.

    Reads settings.model to determine provider, endpoint, and api_key_ref.
    The api_key_ref is resolved from the secrets store here — never stored on
    the returned object in plaintext beyond what the OS process memory holds.

    Always returns a single backend (pinned policy).
    Will be superseded by ModelRouter for multi-provider selection.

    Args:
        settings: a loaded Settings instance, or None to load from config.json.

    Returns:
        A configured ModelBackend ready to accept get_reasoning() calls.

    Raises:
        ValueError: if provider is unknown, endpoint is missing, or
                    api_key_ref resolution fails.
    """
    if settings is None:
        from ma_app.config.settings import Settings
        settings = Settings.load()

    cfg = settings.model
    provider = (cfg.provider or "").lower().strip()

    if not provider:
        raise ValueError(
            "model.provider is not configured. "
            "Set it in config.json under 'model.provider' "
            "(supported values: 'xai', 'generic')."
        )

    if not cfg.endpoint:
        raise ValueError(
            f"model.endpoint is not configured for provider '{provider}'. "
            "Set it in config.json under 'model.endpoint'."
        )

    timeout = getattr(cfg, "request_timeout_seconds", 30)

    if provider == "xai":
        return InternalModelBackend(
            endpoint=cfg.endpoint,
            api_key_ref=cfg.api_key_ref,
            timeout_seconds=timeout,
        )

    if provider == "generic":
        return GenericApiModelBackend(
            endpoint=cfg.endpoint,
            api_key_ref=cfg.api_key_ref,
            timeout_seconds=timeout,
        )

    raise ValueError(
        f"Unknown model provider: {provider!r}. "
        "Supported values for: 'xai', 'generic'. "
        "Multi-provider routing is implemented"
    )