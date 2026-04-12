# /Memory-Archive/ma-app/ma_app/model/pipeline.py

from __future__ import annotations

import logging
import threading
import time
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass, field
from typing import Optional

from ma_app.config.settings import Settings
from ma_app.ipc.client import IPCClient, IPCError
from ma_app.model.backend import (
    ModelBackend,
    NonRetryableError,
    ReasoningResult,
    RetryableError,
    StepData,
)
from ma_app.model.rate_limiter import RateLimiter
from ma_app.model.router import ModelRouter, _BackendEntry

_log = logging.getLogger(__name__)

_BASE_BACKOFF_S = 1.0
_MAX_BACKOFF_S = 30.0


def _backoff(attempt: int) -> float:
    return min(_BASE_BACKOFF_S * (2 ** (attempt - 1)), _MAX_BACKOFF_S)


@dataclass
class _SessionState:
    consecutive_failures: int = 0
    degraded: bool = False
    open_circuits: set[str] = field(default_factory=set)


class ReasoningPipeline:
    """
    Async reasoning pipeline for automated mode.

    Receives StepReadyForReasoning IPC messages via submit(), calls the VLM
    router in a thread pool, and sends ReasoningResult back to ma-core over IPC.

    Routing model:
        Global router  — built from config.model at startup. Used for sessions
                         that did NOT receive per-session VLM config at
                         registration time.
        Per-session router — built by set_session_config() when the StartWatch
                         response includes model_provider + (optional) fallback
                         fields. Stored in _session_routers keyed by session_id.
                         Always uses the 'fallback' policy when both providers
                         are present, 'pinned' when only a primary exists.

    Circuit breaker state is per-session only. Neither router ever holds mutable
    circuit state. All circuit tracking lives in _SessionState.open_circuits.
    A failure on Session A never alters routing decisions for Session B.

    Thread safety: all public methods may be called from any thread.
    """

    def __init__(self, router: ModelRouter, settings: Settings) -> None:
        cfg = settings.model
        self._router = router
        self._settings = settings
        self._rate_limiter = RateLimiter(
            requests_per_minute=cfg.requests_per_minute,
            token_budget_per_hour=cfg.token_budget_per_hour,
        )
        max_workers = min(max(cfg.requests_per_minute, 1), 32)
        self._executor = ThreadPoolExecutor(
            max_workers=max_workers,
            thread_name_prefix="ma-vlm",
        )
        self._session_states: dict[str, _SessionState] = {}
        self._session_providers: dict[str, str] = {}
        self._session_routers: dict[str, ModelRouter] = {}
        self._state_lock = threading.Lock()
        self._shutdown_event = threading.Event()
        self._degraded_step_start: dict[str, int] = {}

    def set_session_provider(self, session_id: str, provider_name: str) -> None:
        """
        Register the provider hint for a session (global-router path).

        Called when a session has no per-session VLM config — the global router
        will use provider_name as the hint for pinned and fallback policies.
        Silently ignored if provider_name is empty.
        """
        if provider_name:
            with self._state_lock:
                self._session_providers[session_id] = provider_name

    def set_session_config(
        self,
        session_id: str,
        primary_provider: str,
        primary_endpoint: str,
        primary_key_ref: str,
        fallback_provider: str,
        fallback_endpoint: str,
        fallback_key_ref: str,
    ) -> None:
        """
        Register per-session VLM config and build a session-specific router.

        Called from the `automated` CLI command after receiving the WatchStarted
        response, which includes the session's VLM config from the SessionRecord.

        If both primary and fallback are configured: builds a 2-entry router with
        'fallback' policy. Session uses its own provider pair exclusively — the
        global router is never consulted for this session.

        If only primary is configured: builds a 1-entry router with 'pinned'
        policy, keeping the session isolated from global pool changes.

        If neither is configured: falls back to registering provider_name via
        set_session_provider() so the global router handles the session as before.

        The 2-provider maximum is enforced upstream in ma-core's RegisterSession
        handler. This method trusts that at most two providers arrive here.
        """
        if not primary_provider:
            return

        timeout = getattr(self._settings.model, "request_timeout_seconds", 30)

        from ma_app.model import _instantiate_backend

        primary_backend = _instantiate_backend(
            name=primary_provider,
            endpoint=primary_endpoint,
            api_key_ref=primary_key_ref,
            timeout_seconds=timeout,
        )
        entries = [_BackendEntry(name=primary_provider, backend=primary_backend, priority=1)]

        if fallback_provider:
            fallback_backend = _instantiate_backend(
                name=fallback_provider,
                endpoint=fallback_endpoint,
                api_key_ref=fallback_key_ref,
                timeout_seconds=timeout,
            )
            entries.append(
                _BackendEntry(name=fallback_provider, backend=fallback_backend, priority=2)
            )
            policy = "fallback"
        else:
            policy = "pinned"

        session_router = ModelRouter(entries=entries, policy=policy)

        with self._state_lock:
            self._session_routers[session_id] = session_router
            self._session_providers[session_id] = primary_provider

        _log.info(
            "Per-session router built: session=%s primary=%s fallback=%s policy=%s",
            session_id,
            primary_provider,
            fallback_provider or "(none)",
            policy,
        )

    def _router_for(self, session_id: str) -> ModelRouter:
        """Return the per-session router if one was built, otherwise the global router."""
        return self._session_routers.get(session_id, self._router)

    def submit(self, msg: dict) -> None:
        """
        Non-blocking. Deserialises the StepReadyForReasoning message and
        submits the step to the thread pool for async VLM processing.
        """
        if self._shutdown_event.is_set():
            return

        try:
            step_data = StepData.from_ipc_message(msg)
        except Exception as e:
            _log.warning("ReasoningPipeline.submit: failed to deserialise step: %s", e)
            return

        if not step_data.session_id or not step_data.step_id:
            _log.warning(
                "ReasoningPipeline.submit: missing session_id or step_id in message"
            )
            return

        with self._state_lock:
            state = self._session_states.setdefault(step_data.session_id, _SessionState())
            if state.degraded:
                _log.debug(
                    "ReasoningPipeline.submit: session %s is degraded — discarding step %d",
                    step_data.session_id,
                    step_data.step_id,
                )
                return

        future: Future = self._executor.submit(self._call_vlm, step_data)
        future.add_done_callback(
            lambda f, sid=step_data.session_id, stepid=step_data.step_id:
                self._on_future_done(sid, stepid, f)
        )

        _log.debug(
            "ReasoningPipeline: submitted step session=%s step=%d",
            step_data.session_id,
            step_data.step_id,
        )

    def mark_session_degraded(self, session_id: str) -> None:
        with self._state_lock:
            state = self._session_states.setdefault(session_id, _SessionState())
            state.degraded = True
        _log.info("ReasoningPipeline: session %s marked degraded", session_id)

    def reset_session(self, session_id: str) -> None:
        with self._state_lock:
            self._session_states.pop(session_id, None)
        _log.info("ReasoningPipeline: session %s reset (circuit closed)", session_id)

    def failure_count(self, session_id: str) -> int:
        with self._state_lock:
            state = self._session_states.get(session_id)
            return state.consecutive_failures if state else 0

    def is_degraded(self, session_id: str) -> bool:
        with self._state_lock:
            state = self._session_states.get(session_id)
            return state.degraded if state else False

    def mark_provider_circuit_open(
        self, session_id: str, provider_name: str
    ) -> None:
        """
        Record that provider_name's circuit has opened for this session.
        Operates exclusively on this session's _SessionState — has no
        effect on any other session.
        """
        with self._state_lock:
            state = self._session_states.setdefault(session_id, _SessionState())
            state.open_circuits.add(provider_name)

    def mark_provider_circuit_closed(
        self, session_id: str, provider_name: str
    ) -> None:
        """
        Remove provider_name from this session's open circuit set.
        Operates exclusively on this session's _SessionState.
        """
        with self._state_lock:
            state = self._session_states.get(session_id)
            if state:
                state.open_circuits.discard(provider_name)

    def is_fully_degraded(self, session_id: str) -> bool:
        """
        True when no backend can serve this session — meaning the session
        should transition to reasoning_degraded.

        Uses the session's own router (per-session if configured, global
        otherwise) and its own open_circuits. Never touches any other
        session's state.
        """
        with self._state_lock:
            state = self._session_states.get(session_id)
            if state is None:
                return False
            session_open_circuits = set(state.open_circuits)

        router = self._router_for(session_id)
        provider_hint = self._session_providers.get(session_id, "")
        provider_names = router.provider_names

        if router.policy == "pinned":
            pinned = provider_hint or (provider_names[0] if provider_names else "")
            return bool(pinned) and pinned in session_open_circuits
        else:
            return bool(provider_names) and all(
                n in session_open_circuits for n in provider_names
            )

    def shutdown(self, wait: bool = True, timeout: Optional[float] = 30.0) -> None:
        self._shutdown_event.set()
        if wait and timeout is not None:
            self._executor.shutdown(wait=False, cancel_futures=True)
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                time.sleep(0.1)
        else:
            self._executor.shutdown(wait=wait, cancel_futures=True)

        try:
            self._router.close()
        except Exception:
            pass

        with self._state_lock:
            session_routers = list(self._session_routers.values())
        for r in session_routers:
            try:
                r.close()
            except Exception:
                pass

    def _call_vlm(self, step_data: StepData) -> tuple[ReasoningResult, str]:
        """
        Called in a worker thread. Returns (result, entry_name).
        The entry_name identifies which backend entry handled the step —
        used by _on_future_done for circuit breaker accounting.

        Uses the session's own router (per-session or global). Reads this
        session's open_circuits from _SessionState and passes them as an
        immutable snapshot to router.resolve(). The router never modifies
        session state.
        """
        provider_hint = self._session_providers.get(step_data.session_id, "")
        router = self._router_for(step_data.session_id)

        if self._rate_limiter.is_token_budget_exceeded():
            raise NonRetryableError(
                f"Hourly token budget exceeded — session={step_data.session_id} "
                f"step={step_data.step_id}. Used: "
                f"{self._rate_limiter.tokens_used_this_hour()} tokens."
            )

        if router.policy == "fallback":
            return self._call_vlm_fallback(step_data, provider_hint, router)

        with self._state_lock:
            state = self._session_states.setdefault(step_data.session_id, _SessionState())
            session_open_circuits = set(state.open_circuits)

        backend = router.resolve(step_data.session_id, provider_hint, session_open_circuits)
        if backend is None:
            raise NonRetryableError(
                f"No backend available for provider '{provider_hint}' — "
                "circuit may be open"
            )
        entry_name = router.entry_name_for(backend)
        result = self._call_vlm_with_retries(backend, step_data)
        return result, entry_name

    def _call_vlm_with_retries(
        self, backend: ModelBackend, step_data: StepData
    ) -> ReasoningResult:
        """Retry loop for a single backend. Raises on exhausted retries."""
        cfg = self._settings.model
        last_error: Exception = RuntimeError("no attempts made")

        for attempt in range(1, cfg.max_retries + 1):
            if self._shutdown_event.is_set():
                raise RetryableError("Pipeline is shutting down")

            slot_acquired = self._rate_limiter.acquire_request_slot(timeout_seconds=30.0)
            if not slot_acquired:
                last_error = RetryableError(
                    f"RPM rate limit timeout — session={step_data.session_id} "
                    f"step={step_data.step_id}"
                )
                if attempt < cfg.max_retries:
                    time.sleep(_backoff(attempt))
                continue

            try:
                result = backend.get_reasoning(step_data)
                self._rate_limiter.record_tokens(result.input_tokens, result.output_tokens)
                return result
            except RetryableError as e:
                last_error = e
                _log.warning(
                    "VLM retryable error (attempt %d/%d) session=%s step=%d: %s",
                    attempt,
                    cfg.max_retries,
                    step_data.session_id,
                    step_data.step_id,
                    e,
                )
                if attempt < cfg.max_retries:
                    time.sleep(_backoff(attempt))
            except NonRetryableError:
                raise

        raise last_error  # type: ignore[misc]

    def _call_vlm_fallback(
        self,
        step_data: StepData,
        provider_hint: str,
        router: ModelRouter,
    ) -> tuple[ReasoningResult, str]:
        """
        Fallback policy: try each available backend in priority order.
        On NonRetryableError, mark that provider's circuit open in this
        session's state (not globally) and try the next provider.
        Raises NonRetryableError when all providers are exhausted.

        Accepts the router as an argument so both global and per-session
        routers use the same fallback logic.
        """
        tried: set[str] = set()
        last_error: Exception = NonRetryableError("No providers available in fallback chain")

        while True:
            with self._state_lock:
                state = self._session_states.setdefault(
                    step_data.session_id, _SessionState()
                )
                session_open_circuits = set(state.open_circuits)

            backend = router.resolve(
                step_data.session_id, provider_hint, session_open_circuits
            )
            if backend is None:
                raise NonRetryableError(
                    "All providers in fallback chain have open circuits"
                ) from last_error

            entry_name = router.entry_name_for(backend)
            if entry_name in tried:
                raise last_error

            tried.add(entry_name)

            try:
                result = self._call_vlm_with_retries(backend, step_data)
                return result, entry_name
            except NonRetryableError as e:
                last_error = e
                self.mark_provider_circuit_open(step_data.session_id, entry_name)
                self._schedule_half_open(step_data.session_id, entry_name)
                _log.warning(
                    "Fallback: provider '%s' circuit opened for session %s — "
                    "trying next provider. step=%d",
                    entry_name,
                    step_data.session_id,
                    step_data.step_id,
                )

    def _on_future_done(self, session_id: str, step_id: int, future: Future) -> None:
        exc = future.exception()
        if exc is None:
            result, entry_name = future.result()
            self._reset_failures(session_id)
            self._send_result(session_id, step_id, result, entry_name)
            return

        _log.error("VLM call failed session=%s step=%d: %s", session_id, step_id, exc)
        provider_hint = self._session_providers.get(session_id, "")
        self._increment_failures(session_id, step_id, exc, provider_hint)

    def _reset_failures(self, session_id: str) -> None:
        with self._state_lock:
            state = self._session_states.get(session_id)
            if state:
                state.consecutive_failures = 0

    def _increment_failures(
        self, session_id: str, step_id: int, exc: BaseException, provider_hint: str
    ) -> None:
        with self._state_lock:
            state = self._session_states.setdefault(session_id, _SessionState())
            state.consecutive_failures += 1
            count = state.consecutive_failures

        _log.warning(
            "Consecutive VLM failures: session=%s count=%d step=%d",
            session_id,
            count,
            step_id,
        )
        self._check_circuit(session_id, step_id, count, provider_hint)

    def _check_circuit(
        self, session_id: str, step_id: int, count: int, provider_hint: str
    ) -> None:
        threshold = self._settings.model.circuit_breaker_threshold
        if count < threshold:
            return

        with self._state_lock:
            state = self._session_states.get(session_id)
            if state and state.degraded:
                return

        router = self._router_for(session_id)

        # For pinned/load_balance, mark the provider's circuit open in this
        # session's state. For fallback, _call_vlm_fallback already marks
        # circuits open inline per provider as each one fails — by the time
        # we reach _check_circuit via NonRetryableError propagation, the
        # entire fallback chain is exhausted.
        if router.policy != "fallback":
            self.mark_provider_circuit_open(session_id, provider_hint)
            self._schedule_half_open(session_id, provider_hint)

        if not self.is_fully_degraded(session_id):
            # A fallback provider is still available for this session.
            # Reset the failure counter so the next step gets a fresh attempt.
            with self._state_lock:
                state = self._session_states.get(session_id)
                if state:
                    state.consecutive_failures = 0
            return

        self.mark_session_degraded(session_id)
        self._degraded_step_start[session_id] = step_id

        _log.error(
            "Circuit breaker opened (all providers): session=%s threshold=%d "
            "step_range_start=%d",
            session_id,
            threshold,
            step_id,
        )
        self._send_degraded_ipc(session_id, step_id)

        # For fallback: half-open trials are scheduled per-provider inside
        # _call_vlm_fallback as each provider's circuit opens. For
        # pinned/load_balance, schedule one now for the provider just opened.
        if router.policy != "fallback":
            self._schedule_half_open(session_id, provider_hint)

    def _schedule_half_open(self, session_id: str, entry_name: str) -> None:
        """
        Unified half-open dispatcher.

        Sessions with a per-session router use _schedule_session_provider_half_open
        so the trial pings via that session's specific backend (which may have a
        different endpoint than the global pool).

        Sessions using the global router use _schedule_provider_half_open, which
        manages a single provider-scoped trial shared across all global-router
        sessions on that provider.
        """
        if session_id in self._session_routers:
            self._schedule_session_provider_half_open(session_id, entry_name)
        else:
            self._schedule_provider_half_open(entry_name)

    def _schedule_provider_half_open(self, entry_name: str) -> None:
        """Global-router half-open timer. One trial per provider name."""
        reset_secs = self._settings.model.circuit_breaker_reset_seconds

        def _run() -> None:
            time.sleep(reset_secs)
            if self._shutdown_event.is_set():
                return
            self._try_provider_half_open(entry_name)

        threading.Thread(
            target=_run,
            daemon=True,
            name=f"ma-cb-{entry_name[:12]}",
        ).start()

    def _schedule_session_provider_half_open(
        self, session_id: str, entry_name: str
    ) -> None:
        """Per-session half-open timer. Pings via the session's own router."""
        reset_secs = self._settings.model.circuit_breaker_reset_seconds

        def _run() -> None:
            time.sleep(reset_secs)
            if self._shutdown_event.is_set():
                return
            self._try_session_provider_half_open(session_id, entry_name)

        threading.Thread(
            target=_run,
            daemon=True,
            name=f"ma-cb-{session_id[:8]}-{entry_name[:8]}",
        ).start()

    def _try_provider_half_open(self, entry_name: str) -> None:
        """
        Global-router half-open trial.

        Checks if any global-router session still has this provider's circuit
        open. If alive, removes the circuit from all affected sessions and
        recovers those that are no longer fully degraded. If dead, reschedules.
        """
        if self._shutdown_event.is_set():
            return

        with self._state_lock:
            any_open = any(
                entry_name in state.open_circuits
                for sid, state in self._session_states.items()
                if sid not in self._session_routers
            )
        if not any_open:
            return

        _log.info("Circuit half-open trial (global): provider=%s", entry_name)
        alive = self._router.ping_entry(entry_name)

        if alive:
            _log.info(
                "Circuit closed (global): provider=%s — sessions may resume", entry_name
            )
            sessions_to_check: list[str] = []
            with self._state_lock:
                for sid, state in self._session_states.items():
                    if sid not in self._session_routers and entry_name in state.open_circuits:
                        state.open_circuits.discard(entry_name)
                        if state.degraded:
                            sessions_to_check.append(sid)

            for session_id in sessions_to_check:
                if not self.is_fully_degraded(session_id):
                    with self._state_lock:
                        state = self._session_states.get(session_id)
                        if state and state.degraded:
                            state.degraded = False
                            state.consecutive_failures = 0
                        else:
                            continue
                    self._degraded_step_start.pop(session_id, None)
                    _log.info(
                        "Circuit closed: session=%s — StepReadyForReasoning resuming",
                        session_id,
                    )
                    self._send_circuit_reset_ipc(session_id)
        else:
            _log.warning(
                "Circuit trial failed (global): provider=%s — rescheduling", entry_name
            )
            self._schedule_provider_half_open(entry_name)

    def _try_session_provider_half_open(
        self, session_id: str, entry_name: str
    ) -> None:
        """
        Per-session half-open trial.

        Pings via the session's own router. Only affects this session.
        If the session's router has been cleaned up (session ended), exits
        silently.
        """
        if self._shutdown_event.is_set():
            return

        with self._state_lock:
            session_router = self._session_routers.get(session_id)
            state = self._session_states.get(session_id)
            if session_router is None or state is None:
                return
            if entry_name not in state.open_circuits:
                return

        _log.info(
            "Circuit half-open trial (per-session): session=%s provider=%s",
            session_id,
            entry_name,
        )
        alive = session_router.ping_entry(entry_name)

        if alive:
            self.mark_provider_circuit_closed(session_id, entry_name)
            _log.info(
                "Circuit closed (per-session): session=%s provider=%s",
                session_id,
                entry_name,
            )
            if not self.is_fully_degraded(session_id):
                with self._state_lock:
                    state = self._session_states.get(session_id)
                    if state and state.degraded:
                        state.degraded = False
                        state.consecutive_failures = 0
                    else:
                        return
                self._degraded_step_start.pop(session_id, None)
                _log.info(
                    "Circuit closed: session=%s — StepReadyForReasoning resuming",
                    session_id,
                )
                self._send_circuit_reset_ipc(session_id)
        else:
            _log.warning(
                "Circuit trial failed (per-session): session=%s provider=%s — "
                "rescheduling",
                session_id,
                entry_name,
            )
            self._schedule_session_provider_half_open(session_id, entry_name)

    def _send_degraded_ipc(self, session_id: str, step_range_start: int) -> None:
        msg = {
            "type": "reasoning_degraded",
            "session_id": session_id,
            "step_range_start": step_range_start,
        }
        try:
            with IPCClient() as client:
                client.send(msg)
        except IPCError as e:
            _log.error(
                "Failed to send ReasoningDegraded to ma-core: session=%s: %s",
                session_id,
                e,
            )

    def _send_circuit_reset_ipc(self, session_id: str) -> None:
        msg = {"type": "circuit_reset", "session_id": session_id}
        try:
            with IPCClient() as client:
                client.send(msg)
        except IPCError as e:
            _log.error(
                "Failed to send CircuitReset to ma-core: session=%s: %s",
                session_id,
                e,
            )

    def _send_result(
        self,
        session_id: str,
        step_id: int,
        result: ReasoningResult,
        provider_name: str = "",
    ) -> None:
        msg = {
            "type": "reasoning_result",
            "session_id": session_id,
            "step_id": step_id,
            "reasoning": result.reasoning,
            "source": "model",
            "provider": provider_name,
            "model_id": result.model_id,
            "api_version": result.api_version,
            "input_tokens": result.input_tokens,
            "output_tokens": result.output_tokens,
            "latency_ms": result.latency_ms,
            "action_intent": result.action_intent,
            "confidence": result.confidence,
            "keyboard_visual_annotation": result.keyboard_visual_annotation,
        }
        try:
            with IPCClient() as client:
                response = client.send(msg)
            resp_type = response.get("type", "")
            if resp_type == "reasoning_result_accepted":
                _log.debug(
                    "ReasoningResult accepted: session=%s step=%d",
                    session_id,
                    step_id,
                )
            elif resp_type == "error":
                code = response.get("code", "")
                if code == "NOT_IMPLEMENTED":
                    _log.debug(
                        "ReasoningResult acknowledged (T8.6 not yet active) "
                        "session=%s step=%d",
                        session_id,
                        step_id,
                    )
                else:
                    _log.warning(
                        "ReasoningResult rejected by ma-core: session=%s step=%d "
                        "code=%s message=%s",
                        session_id,
                        step_id,
                        code,
                        response.get("message", ""),
                    )
            else:
                _log.debug(
                    "ReasoningResult response: session=%s step=%d type=%s",
                    session_id,
                    step_id,
                    resp_type,
                )
        except IPCError as e:
            _log.error(
                "Failed to send ReasoningResult to ma-core: session=%s step=%d: %s",
                session_id,
                step_id,
                e,
            )