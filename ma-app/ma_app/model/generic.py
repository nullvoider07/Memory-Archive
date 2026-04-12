# /Memory-Archive/ma-app/ma_app/model/generic.py

from __future__ import annotations

import logging
import time
from typing import Optional

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


class GenericApiModelBackend(ModelBackend):
    """
    VLM backend for any HTTP endpoint that speaks the Memory Archive reasoning protocol.

    The default wire format is identical to InternalModelBackend. Field name
    overrides allow adaptation to APIs with different naming conventions without
    requiring code changes:

        request_field_map   maps our canonical request field names → API field names
        response_field_map  maps the API's response field names → our canonical names

    Only fields that differ from the default need to appear in the map.
    Fields absent from the map pass through with their canonical name unchanged.

    Example — an API that calls the frame "image" instead of "at_frame" and returns
    "result" instead of "reasoning":
        request_field_map  = {"at_frame": "image"}
        response_field_map = {"result": "reasoning"}

    Thread safety: httpx.Client is thread-safe after construction.
    """

    def __init__(
        self,
        endpoint: str,
        api_key_ref: str,
        timeout_seconds: int = 30,
        request_field_map: Optional[dict[str, str]] = None,
        response_field_map: Optional[dict[str, str]] = None,
    ) -> None:
        if not endpoint.startswith("https://"):
            raise ValueError(
                f"GenericApiModelBackend: endpoint must use HTTPS, got: {endpoint!r}"
            )
        self._endpoint = endpoint.rstrip("/")
        self._api_key = resolve_api_key(api_key_ref)
        self._client = httpx.Client(
            timeout=httpx.Timeout(timeout=float(timeout_seconds), connect=10.0),
            follow_redirects=False,
            verify=True,
            headers={"User-Agent": "memory-archive/0.1"},
        )
        self._req_map: dict[str, str] = request_field_map or {}
        # response_field_map: api_field_name → canonical_name
        self._resp_map: dict[str, str] = response_field_map or {}

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
                f"{_MAX_RESPONSE_BYTES} bytes — possible schema mismatch"
            )

        try:
            raw_body = response.json()
        except Exception as e:
            raise NonRetryableError(
                f"VLM response for step {step_data.step_id} is not valid JSON: {e}"
            ) from e

        body = self._normalize_response(raw_body)
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

    def _remap_request_field(self, canonical: str) -> str:
        return self._req_map.get(canonical, canonical)

    def _build_request(self, step_data: StepData) -> dict:
        rf = self._remap_request_field
        payload: dict = {
            rf("step_id"): step_data.step_id,
            rf("session_id"): step_data.session_id,
            rf("action_type"): step_data.action_type,
            rf("action_subtype"): step_data.action_subtype,
            rf("converted_command"): step_data.converted_command,
            rf("at_frame"): step_data.at_frame_b64,
            rf("context"): [
                {
                    "step_id": c.step_id,
                    "command": c.converted_command,
                    "reasoning": c.reasoning,
                }
                for c in step_data.context_steps
            ],
        }
        if step_data.is_keyboard:
            payload[rf("before_frame")] = step_data.before_frame_b64
            payload[rf("after_frame")] = step_data.after_frame_b64
        return payload

    def _normalize_response(self, raw: dict) -> dict:
        """
        Translate API response field names to canonical names using response_field_map.
        Fields not in the map pass through with their original name unchanged.
        """
        if not self._resp_map:
            return raw
        out: dict = {}
        for api_name, value in raw.items():
            canonical = self._resp_map.get(api_name, api_name)
            out[canonical] = value
        return out