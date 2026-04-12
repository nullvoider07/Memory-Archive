# /Memory-Archive/ma-app/ma_app/model/backend.py

from __future__ import annotations

import base64
import time
from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from typing import Optional


_MAX_RESPONSE_BYTES = 1 * 1024 * 1024  # 1 MB — reasoning responses are small

_RETRYABLE_STATUS_CODES = {429, 503, 502, 504}
_NON_RETRYABLE_STATUS_CODES = {400, 401, 403, 422}


class ModelError(Exception):
    """Base for all VLM backend errors."""


class RetryableError(ModelError):
    """
    Transient failure — exponential backoff, retries permitted.
    HTTP 429 (rate limited), 503/502/504 (service unavailable), network timeout.
    Does not count toward the circuit breaker threshold until max_retries is exhausted.
    """
    def __init__(self, message: str, status_code: int = 0) -> None:
        super().__init__(message)
        self.status_code = status_code


class NonRetryableError(ModelError):
    """
    Permanent failure — counts toward circuit breaker threshold immediately.
    HTTP 400 (bad request), 401 (auth failure), 403 (forbidden), 422 (schema mismatch),
    or a response that fails schema validation.
    """
    def __init__(self, message: str, status_code: int = 0) -> None:
        super().__init__(message)
        self.status_code = status_code


@dataclass
class ContextStep:
    """One prior step's context included in a StepReadyForReasoning payload."""
    step_id: int
    converted_command: str
    reasoning: str


@dataclass
class StepData:
    """
    All data needed to reason about one capture step.
    Constructed from a StepReadyForReasoning IPC push event.

    Frame bytes are always base64-encoded on the wire (from Rust).
    before_frame_bytes and after_frame_bytes are only populated for keyboard steps.
    """
    session_id: str
    step_id: int
    action_type: str          # "mouse" | "keyboard"
    action_subtype: str       # "left", "right", "double", "press", "type", ...
    converted_command: str
    at_frame_bytes: bytes
    before_frame_bytes: bytes = field(default_factory=bytes)
    after_frame_bytes: bytes = field(default_factory=bytes)
    context_steps: list[ContextStep] = field(default_factory=list)

    @property
    def is_keyboard(self) -> bool:
        return self.action_type == "keyboard"

    @property
    def at_frame_b64(self) -> str:
        return base64.b64encode(self.at_frame_bytes).decode("ascii")

    @property
    def before_frame_b64(self) -> Optional[str]:
        if not self.before_frame_bytes:
            return None
        return base64.b64encode(self.before_frame_bytes).decode("ascii")

    @property
    def after_frame_b64(self) -> Optional[str]:
        if not self.after_frame_bytes:
            return None
        return base64.b64encode(self.after_frame_bytes).decode("ascii")

    @classmethod
    def from_ipc_message(cls, msg: dict) -> "StepData":
        """
        Deserialize from a StepReadyForReasoning IPC message dict.
        Decodes base64 frame fields. Missing or malformed frame fields become empty bytes.
        """
        def decode_b64(val: str) -> bytes:
            if not val:
                return b""
            try:
                return base64.b64decode(val)
            except Exception:
                return b""

        context = [
            ContextStep(
                step_id=int(c.get("step_id", 0)),
                converted_command=str(c.get("converted_command", "")),
                reasoning=str(c.get("reasoning", "")),
            )
            for c in msg.get("context_steps", [])
            if isinstance(c, dict)
        ]

        return cls(
            session_id=str(msg.get("session_id", "")),
            step_id=int(msg.get("step_id", 0)),
            action_type=str(msg.get("action_type", "")),
            action_subtype=str(msg.get("action_subtype", "")),
            converted_command=str(msg.get("converted_command", "")),
            at_frame_bytes=decode_b64(msg.get("at_frame_bytes", "")),
            before_frame_bytes=decode_b64(msg.get("before_frame_bytes", "")),
            after_frame_bytes=decode_b64(msg.get("after_frame_bytes", "")),
            context_steps=context,
        )


@dataclass
class ReasoningResult:
    """
    Structured output from a VLM reasoning call.
    Used to build the reasoning.jsonl entry for a step.
    """
    step_id: int
    reasoning: str
    model_id: str
    api_version: str
    input_tokens: int
    output_tokens: int
    latency_ms: int
    source: str = "model"
    action_intent: Optional[str] = None
    confidence: Optional[float] = None
    keyboard_visual_annotation: Optional[dict] = None


def _classify_http_error(status_code: int, step_id: int) -> None:
    """
    Raise the appropriate error type for a non-2xx HTTP response.

    Retryable:     429, 502, 503, 504, and any unknown status code
    Non-retryable: 400, 401, 403, 422

    Called by both InternalModelBackend and GenericApiModelBackend so any
    future changes to the classification policy apply in one place.
    """
    if status_code in _NON_RETRYABLE_STATUS_CODES:
        raise NonRetryableError(
            f"VLM returned HTTP {status_code} for step {step_id} — non-retryable",
            status_code=status_code,
        )
    if status_code in _RETRYABLE_STATUS_CODES:
        raise RetryableError(
            f"VLM returned HTTP {status_code} for step {step_id} — retryable",
            status_code=status_code,
        )
    raise RetryableError(
        f"VLM returned unexpected HTTP {status_code} for step {step_id} — treating as retryable",
        status_code=status_code,
    )


def _parse_vlm_response(
    body: dict,
    step_id: int,
    measured_latency_ms: int,
) -> ReasoningResult:
    """
    Validate and deserialize a VLM JSON response body into a ReasoningResult.

    Raises NonRetryableError if required fields are missing, empty, or the wrong type.
    Soft validation (confidence, keyboard_visual_annotation) silently discards bad values
    rather than failing the entire step.

    Security notes:
    - reasoning is stripped of leading/trailing whitespace
    - confidence is validated to [0.0, 1.0]; out-of-range values become None
    - keyboard_visual_annotation must be a dict; any other type becomes None
    - action_intent must be a non-empty string or None
    - server-reported latency_ms is used only if plausible (0 < value < 5 minutes);
      otherwise the client-measured value is used
    """
    reasoning = body.get("reasoning")
    if not isinstance(reasoning, str) or not reasoning.strip():
        raise NonRetryableError(
            f"VLM response for step {step_id} has missing or empty 'reasoning' field"
        )

    model_id = body.get("model_id")
    if not isinstance(model_id, str) or not model_id.strip():
        raise NonRetryableError(
            f"VLM response for step {step_id} has missing or empty 'model_id' field"
        )

    server_latency = body.get("latency_ms")
    latency_ms = (
        int(server_latency)
        if isinstance(server_latency, (int, float)) and 0 < server_latency < 300_000
        else measured_latency_ms
    )

    confidence = body.get("confidence")
    if confidence is not None:
        try:
            confidence = float(confidence)
            if not (0.0 <= confidence <= 1.0):
                confidence = None
        except (TypeError, ValueError):
            confidence = None

    action_intent = body.get("action_intent")
    if not isinstance(action_intent, str) or not action_intent.strip():
        action_intent = None

    kva = body.get("keyboard_visual_annotation")
    if kva is not None and not isinstance(kva, dict):
        kva = None

    return ReasoningResult(
        step_id=step_id,
        reasoning=reasoning.strip(),
        model_id=model_id.strip(),
        api_version=str(body.get("api_version", "")).strip(),
        input_tokens=int(body.get("input_tokens", 0)),
        output_tokens=int(body.get("output_tokens", 0)),
        latency_ms=latency_ms,
        action_intent=action_intent,
        confidence=confidence,
        keyboard_visual_annotation=kva,
    )


class ModelBackend(ABC):
    """
    Abstract base for VLM reasoning backends.

    Each backend manages its own HTTP session, auth headers, and provider-specific
    request/response format. The async pipeline (T8.5) calls get_reasoning() and
    is transparent to which provider is active.

    Thread safety: get_reasoning() and ping() must be safe to call concurrently
    from multiple threads. The httpx.Client used by concrete implementations
    is thread-safe after construction.

    Lifecycle: call close() when the session ends to release the connection pool.
    """

    @abstractmethod
    def get_reasoning(self, step_data: StepData) -> ReasoningResult:
        """
        Call the VLM API and return structured reasoning for one step.

        Raises:
            RetryableError: transient failure — pipeline should retry with backoff
            NonRetryableError: permanent failure — pipeline should open circuit breaker
        """

    @abstractmethod
    def ping(self) -> bool:
        """
        Check whether the VLM API endpoint is reachable.
        Returns True if the health endpoint responds with a 2xx status.
        Never raises — returns False on any error.
        """

    def close(self) -> None:
        """
        Release any resources held by this backend (connection pool, etc.).
        Default is a no-op; concrete implementations override as needed.
        """