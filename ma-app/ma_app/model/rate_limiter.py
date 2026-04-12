# /Memory-Archive/ma-app/ma_app/model/rate_limiter.py

from __future__ import annotations

import threading
import time
from collections import deque


class RateLimiter:
    """
    Thread-safe rate limiter for the VLM reasoning pipeline.

    Enforces two independent limits:
        requests_per_minute — sliding 60-second window on request count
        token_budget_per_hour — sliding 3600-second window on token consumption

    acquire_request_slot() must be called before each VLM API call.
    record_tokens() must be called after each successful call to track usage.
    is_token_budget_exceeded() should be checked before acquire_request_slot()
    to skip calls that would burn budget when the hourly limit is reached.
    """

    def __init__(self, requests_per_minute: int, token_budget_per_hour: int) -> None:
        if requests_per_minute <= 0:
            raise ValueError("requests_per_minute must be positive")
        if token_budget_per_hour <= 0:
            raise ValueError("token_budget_per_hour must be positive")

        self._rpm = requests_per_minute
        self._token_budget = token_budget_per_hour
        self._request_timestamps: deque[float] = deque()
        self._token_events: deque[tuple[float, int]] = deque()
        self._lock = threading.Lock()

    def acquire_request_slot(self, timeout_seconds: float = 30.0) -> bool:
        """
        Block until a request slot is available within the RPM limit.

        Returns True if a slot was acquired, False if timeout_seconds elapsed
        without a slot becoming available (caller should treat as retryable).

        Thread-safe: multiple worker threads may call this concurrently.
        """
        deadline = time.monotonic() + timeout_seconds
        while True:
            with self._lock:
                now = time.monotonic()
                self._evict_old_requests(now)
                if len(self._request_timestamps) < self._rpm:
                    self._request_timestamps.append(now)
                    return True
                # Compute sleep duration until the oldest request falls out of the window
                wait = 60.0 - (now - self._request_timestamps[0]) + 0.01
            remaining = deadline - time.monotonic()
            if remaining <= 0 or wait > remaining:
                return False
            time.sleep(min(wait, 0.25))

    def record_tokens(self, input_tokens: int, output_tokens: int) -> None:
        """
        Record token usage after a successful VLM call.
        Must be called from the same thread that called acquire_request_slot,
        or any thread — this method is thread-safe.
        """
        with self._lock:
            now = time.monotonic()
            total = max(0, input_tokens) + max(0, output_tokens)
            if total > 0:
                self._token_events.append((now, total))
            self._evict_old_tokens(now)

    def is_token_budget_exceeded(self) -> bool:
        """
        True if the token consumption in the last 3600 seconds meets or exceeds
        the configured hourly budget. Check this before acquiring a request slot
        to avoid making an API call that would be counted against an exhausted budget.
        """
        with self._lock:
            now = time.monotonic()
            self._evict_old_tokens(now)
            total = sum(tokens for _, tokens in self._token_events)
            return total >= self._token_budget

    def tokens_used_this_hour(self) -> int:
        """Return the total tokens consumed in the current sliding hour window."""
        with self._lock:
            now = time.monotonic()
            self._evict_old_tokens(now)
            return sum(tokens for _, tokens in self._token_events)

    def requests_in_current_minute(self) -> int:
        """Return the number of requests made in the current sliding 60-second window."""
        with self._lock:
            now = time.monotonic()
            self._evict_old_requests(now)
            return len(self._request_timestamps)

    def _evict_old_requests(self, now: float) -> None:
        while self._request_timestamps and now - self._request_timestamps[0] >= 60.0:
            self._request_timestamps.popleft()

    def _evict_old_tokens(self, now: float) -> None:
        while self._token_events and now - self._token_events[0][0] >= 3600.0:
            self._token_events.popleft()