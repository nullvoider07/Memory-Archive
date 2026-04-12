# /Memory-Archive/ma-app/ma_app/model/internal_model.py

from __future__ import annotations

import logging
import time

import httpx

from ma_app.model.backend import (
    ModelBackend,
    NonRetryableError,
    ReasoningResult,
    RetryableError,
    StepData,
    _MAX_RESPONSE_BYTES,
    _classify_http_error,
    _parse_vlm_response,
)
from ma_app.model.secrets import resolve_api_key

_log = logging.getLogger(__name__)


class InternalModelBackend(ModelBackend):
    """
    VLM backend for Internal reasoning model API.

    Wire protocol — POST {endpoint}
    ─────────────────────────────────
    Headers:
        Authorization: Bearer {api_key}
        Content-Type: application/json

    Request body:
    {
        "step_id": <int>,
        "session_id": "<str>",
        "action_type": "mouse" | "keyboard",
        "action_subtype": "<str>",
        "converted_command": "<str>",
        "at_frame": "<base64>",
        "before_frame": "<base64 | null>",   # keyboard steps only
        "after_frame":  "<base64 | null>",   # keyboard steps only
        "context": [
            { "step_id": <int>, "command": "<str>", "reasoning": "<str>" }
        ]
    }

    Response body (HTTP 200):
    {
        "step_id": <int>,
        "reasoning": "<str>",
        "action_intent": "<str | null>",
        "confidence": <float | null>,
        "keyboard_visual_annotation": <object | null>,
        "model_id": "<str>",
        "api_version": "<str>",
        "input_tokens": <int>,
        "output_tokens": <int>,
        "latency_ms": <int>
    }

    Health check — GET {endpoint}/health → HTTP 200 means alive.

    Thread safety: httpx.Client is thread-safe after construction.
    """

    def __init__(
        self,
        endpoint: str,
        api_key_ref: str,
        timeout_seconds: int = 30,
    ) -> None:
        if not endpoint.startswith("https://"):
            raise ValueError(
                f"InteralModelBackend: endpoint must use HTTPS, got: {endpoint!r}"
            )
        self._endpoint = endpoint.rstrip("/")
        self._api_key = resolve_api_key(api_key_ref)
        self._client = httpx.Client(
            timeout=httpx.Timeout(timeout=float(timeout_seconds), connect=10.0),
            follow_redirects=False,
            verify=True,
            headers={"User-Agent": "memory-archive/0.1"},
        )

    def get_reasoning(self, step_data: StepData) -> ReasoningResult:
        payload = self._build_request(step_data)
        headers = {}
        if self._api_key:
            headers["Authorization"] = f"Bearer {self._api_key}"

        t_start = time.monotonic()
        try:
            response = self._client.post(
                self._endpoint,
                json=payload,
                headers=headers,
            )
        except httpx.TimeoutException as e:
            raise RetryableError(
                f"VLM request timed out for step {step_data.step_id}: {e}"
            ) from e
        except httpx.NetworkError as e:
            raise RetryableError(
                f"Network error calling VLM for step {step_data.step_id}: {e}"
            ) from e
        except httpx.HTTPError as e:
            raise RetryableError(
                f"HTTP error calling VLM for step {step_data.step_id}: {e}"
            ) from e

        elapsed_ms = int((time.monotonic() - t_start) * 1000)

        if not response.is_success:
            _classify_http_error(response.status_code, step_data.step_id)

        if len(response.content) > _MAX_RESPONSE_BYTES:
            raise NonRetryableError(
                f"VLM response for step {step_data.step_id} exceeds "
                f"{_MAX_RESPONSE_BYTES} bytes — possible schema mismatch or runaway response"
            )

        try:
            body = response.json()
        except Exception as e:
            raise NonRetryableError(
                f"VLM response for step {step_data.step_id} is not valid JSON: {e}"
            ) from e

        return _parse_vlm_response(body, step_data.step_id, elapsed_ms)

    def ping(self) -> bool:
        try:
            headers = {}
            if self._api_key:
                headers["Authorization"] = f"Bearer {self._api_key}"
            response = self._client.get(
                f"{self._endpoint}/health",
                headers=headers,
                timeout=httpx.Timeout(5.0),
            )
            return response.is_success
        except Exception:
            return False

    def close(self) -> None:
        self._client.close()

    def _build_request(self, step_data: StepData) -> dict:
        payload: dict = {
            "step_id": step_data.step_id,
            "session_id": step_data.session_id,
            "action_type": step_data.action_type,
            "action_subtype": step_data.action_subtype,
            "converted_command": step_data.converted_command,
            "at_frame": step_data.at_frame_b64,
            "context": [
                {
                    "step_id": c.step_id,
                    "command": c.converted_command,
                    "reasoning": c.reasoning,
                }
                for c in step_data.context_steps
            ],
        }
        if step_data.is_keyboard:
            payload["before_frame"] = step_data.before_frame_b64
            payload["after_frame"] = step_data.after_frame_b64
        return payload