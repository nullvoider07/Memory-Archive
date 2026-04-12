# /Memory-Archive/ma-app/ma_app/model/router.py

from __future__ import annotations

from dataclasses import dataclass
from typing import Optional

from ma_app.model.backend import ModelBackend


@dataclass
class _BackendEntry:
    name: str
    backend: ModelBackend
    priority: int


class ModelRouter:
    """
    Stateless VLM routing policy engine.

    Routes VLM calls across a pool of ModelBackend instances.
    Holds NO mutable circuit state — all circuit breaker state lives
    exclusively in per-session _SessionState inside ReasoningPipeline.
    A circuit open event on Session A has zero effect on routing
    decisions for Session B. This is a hard architectural invariant.

    Three policies (set at construction time):

    pinned:
        Each session is pinned to the backend whose name matches its
        provider_hint. If the hint is absent the highest-priority entry
        is used. Circuit open → returns None → session degrades
        immediately. Preserves behaviour when the pool contains
        exactly one entry.

    fallback:
        Backends tried in ascending priority order. Any entry in
        session_open_circuits is skipped. Returns None only when all
        entries are in the open set, meaning the full fallback chain is
        exhausted for this session.

    load_balance:
        Consistent assignment per session via abs(hash(session_id)) mod
        len(available). Produces a stable assignment for the session's
        lifetime without any stored state. Backends in
        session_open_circuits are excluded from the available pool,
        causing automatic reassignment to the next available backend.
    """

    def __init__(self, entries: list[_BackendEntry], policy: str) -> None:
        if not entries:
            raise ValueError("ModelRouter requires at least one backend entry")
        if policy not in ("pinned", "fallback", "load_balance"):
            raise ValueError(
                f"Unknown routing_policy {policy!r}. "
                "Expected: 'pinned', 'fallback', or 'load_balance'."
            )

        self._entries = entries
        self._by_name: dict[str, _BackendEntry] = {e.name: e for e in entries}
        self._policy = policy
        self._priority_order: list[str] = [
            e.name for e in sorted(entries, key=lambda e: (e.priority, e.name))
        ]

    @property
    def policy(self) -> str:
        return self._policy

    @property
    def provider_names(self) -> list[str]:
        """Ordered list of provider names (ascending priority, then name)."""
        return list(self._priority_order)

    def resolve(
        self,
        session_id: str,
        provider_hint: str,
        session_open_circuits: set[str],
    ) -> Optional[ModelBackend]:
        """
        Return the backend to use for this session step, or None when no
        backend is available (all relevant circuits open for this session).

        session_id         — used by the load_balance policy for stable
                             consistent hashing; ignored by pinned/fallback.
        provider_hint      — preferred provider name; ignored when empty.
        session_open_circuits — snapshot of _SessionState.open_circuits for
                             this session. The router never mutates this set.

        Thread-safe: the router holds no mutable state and requires no lock.
        """
        if self._policy == "pinned":
            return self._resolve_pinned(provider_hint, session_open_circuits)
        elif self._policy == "fallback":
            return self._resolve_fallback(provider_hint, session_open_circuits)
        else:
            return self._resolve_lb(session_id, session_open_circuits)

    def _resolve_pinned(
        self, provider_hint: str, session_open_circuits: set[str]
    ) -> Optional[ModelBackend]:
        entry = self._by_name.get(provider_hint)
        if entry is None:
            entry = (
                self._by_name.get(self._priority_order[0])
                if self._priority_order
                else None
            )
        if entry is None or entry.name in session_open_circuits:
            return None
        return entry.backend

    def _resolve_fallback(
        self, provider_hint: str, session_open_circuits: set[str]
    ) -> Optional[ModelBackend]:
        order: list[str] = []
        if provider_hint and provider_hint in self._by_name:
            order.append(provider_hint)
        order += [n for n in self._priority_order if n != provider_hint]
        for name in order:
            if name not in session_open_circuits:
                entry = self._by_name.get(name)
                if entry:
                    return entry.backend
        return None

    def _resolve_lb(
        self, session_id: str, session_open_circuits: set[str]
    ) -> Optional[ModelBackend]:
        available = [e for e in self._entries if e.name not in session_open_circuits]
        if not available:
            return None
        idx = abs(hash(session_id)) % len(available)
        return available[idx].backend

    def entry_name_for(self, backend: ModelBackend) -> str:
        """Return the entry name for a specific backend instance."""
        for entry in self._entries:
            if entry.backend is backend:
                return entry.name
        return ""

    def ping_entry(self, entry_name: str) -> bool:
        """Ping a specific backend. Returns False on any exception."""
        entry = self._by_name.get(entry_name)
        if not entry:
            return False
        try:
            return entry.backend.ping()
        except Exception:
            return False

    def close(self) -> None:
        """Release all backends. Called on pipeline shutdown."""
        for entry in self._entries:
            try:
                entry.backend.close()
            except Exception:
                pass