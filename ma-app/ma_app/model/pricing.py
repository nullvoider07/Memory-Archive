# /Memory-Archive/ma-app/ma_app/model/pricing.py
"""
Pricing registry for VLM token cost lookups.

Resolution order for any model_id:
  1. Verified MA manifest — fetched from _MANIFEST_URL at startup, signature-
     checked against the bundled Ed25519 public key, cached locally for 24h.
  2. Bundled baseline table — hardcoded rates compiled at release time. Used
     when network is unavailable or manifest verification fails. Zero runtime
     dependencies; never requires a network call.

Bedrock supplement:
  fetch_bedrock_pricing(model_id, region) queries the AWS Pricing API for
  Bedrock-hosted models. Called explicitly by the cost command; not part of
  the standard lookup() path. IAM requirement: pricing:GetProducts on
  arn:aws:pricing:*:*:*. This permission belongs in a separate IAM policy
  from Bedrock inference permissions.

Security:
  - Manifest URL is hardcoded. The Ed25519 public key is embedded in this
    module as _REGISTRY_PUBLIC_KEY_BYTES and is not configurable. Both
    constraints together prevent DNS-spoofing / BGP-hijack registry poisoning.
  - A manifest with an invalid or missing signature is always rejected.
    The cache or baseline is used instead. A warning is logged and a Prometheus
    counter incremented (via the alert mechanism) so the operator is alerted.

Normalization version-pinning:
  The AWS Bedrock Pricing API response normalization is pinned to
  _BEDROCK_API_FORMAT_VERSION. If AWS changes the response shape, a MA version
  bump is required to update the normalization. The code never attempts to
  adapt to unknown shapes — it returns None on any unexpected structure rather
  than silently computing incorrect costs.
"""

from __future__ import annotations

import json
import logging
import time
from pathlib import Path
from typing import Optional

_log = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

_MANIFEST_URL = "https://pricing.memory-archive.dev/v1/pricing-manifest.json"
_CACHE_PATH = Path.home() / ".memory-archive" / "pricing-cache.json"
_CACHE_TTL_SECONDS = 86_400  # 24 hours

# Ed25519 public key — 32 bytes in raw format.
# The corresponding private key is held offline by the MA team and is used
# to sign each new manifest before publishing. This key is not configurable;
# it is embedded at build time so no config manipulation can override it.
_REGISTRY_PUBLIC_KEY_BYTES = bytes.fromhex(
    "3d7f2a4b8c1e5f9d0b3e6a2c7f4d1b8e5a3c9f6d2b0e7a4c1f8b5d3a6e9c2f50"
)

# Bedrock Pricing API response format version this normalization targets.
# If AWS changes the response shape, bump this constant alongside the parser.
_BEDROCK_API_FORMAT_VERSION = "2026-04"

# ---------------------------------------------------------------------------
# Bundled baseline table
# ---------------------------------------------------------------------------
# Rates per million tokens as of Memory Archive v0.10.0 (2026-04-02).
# This table is the last-resort fallback — no network or signature required.
# Entries: {canonical_model_id: (input_per_million, output_per_million, [aliases])}

_BASELINE: dict[str, tuple[float, float, list[str], bool]] = {
    # xAI Grok API (primary — ALL available models, including older snapshots)
    # Format: (input $/M, output $/M, [aliases], is_vlm_capable)
    "grok-4.20-reasoning": (2.00, 6.00, ["grok-4.20-reasoning", "grok-4.20", "grok-4.20-latest"], True),
    "grok-4.20-non-reasoning": (2.00, 6.00, ["grok-4.20-non-reasoning"], True),
    "grok-4-1-fast-reasoning": (0.20, 0.50, ["grok-4.1-fast-reasoning", "grok-4-1-fast-reasoning", "grok-4-fast-reasoning"], True),
    "grok-4-1-fast-non-reasoning": (0.20, 0.50, ["grok-4.1-fast-non-reasoning", "grok-4-fast-non-reasoning"], True),
    "grok-4-0709": (3.00, 15.00, ["grok-4", "grok-4-latest"], True),
    "grok-3": (3.00, 15.00, ["grok-3"], True),
    "grok-3-mini": (0.30, 0.50, ["grok-3-mini"], True),
    "grok-2-vision-1212": (2.00, 10.00, ["grok-2", "grok-2-vision"], True),

    # OpenAI (ALL active models)
    "gpt-5.4": (2.50, 15.00, ["gpt-5.4-latest", "gpt-5"], True),
    "gpt-5.4-mini": (0.75, 4.50, ["gpt-5.4-mini"], True),
    "gpt-5.4-nano": (0.20, 1.25, ["gpt-5.4-nano"], True),
    "gpt-5.4-pro": (30.00, 180.00, ["gpt-5.4-pro"], True),
    "gpt-realtime-1.5": (4.00, 16.00, ["gpt-realtime"], False),   # audio/voice primary
    "gpt-image-1.5": (5.00, 10.00, ["gpt-image"], True),

    # Anthropic (direct API — ALL Claude models)
    "claude-opus-4.6": (5.00, 25.00, ["claude-opus-4-6", "claude-opus-latest"], True),
    "claude-opus-4.5": (5.00, 25.00, ["claude-opus-4-5"], True),
    "claude-opus-4.1": (15.00, 75.00, ["claude-opus-4-1"], True),
    "claude-opus-4": (15.00, 75.00, ["claude-opus-4"], True),
    "claude-sonnet-4.6": (3.00, 15.00, ["claude-sonnet-4-6", "claude-sonnet-latest"], True),
    "claude-sonnet-4.5": (3.00, 15.00, ["claude-sonnet-4-5"], True),
    "claude-sonnet-4": (3.00, 15.00, ["claude-sonnet-4"], True),
    "claude-sonnet-3.7": (3.00, 15.00, ["claude-sonnet-3.7"], True),
    "claude-haiku-4.5": (1.00, 5.00, ["claude-haiku-4-5", "claude-haiku-latest"], True),
    "claude-haiku-3.5": (0.80, 4.00, ["claude-haiku-3.5"], True),
    "claude-haiku-3": (0.25, 1.25, ["claude-haiku-3"], True),
    "claude-opus-3": (15.00, 75.00, ["claude-opus-3"], True),

    # Google (Gemini API / Vertex AI — ALL models, base rates ≤200k context)
    "gemini-3.1-pro-preview": (2.00, 12.00, ["gemini-3.1-pro", "gemini-3-pro"], True),
    "gemini-3.1-flash-lite-preview": (0.25, 1.50, ["gemini-3.1-flash-lite"], True),
    "gemini-3-flash-preview": (0.50, 3.00, ["gemini-3-flash"], True),
    "gemini-3-pro-preview": (2.00, 12.00, ["gemini-3-pro-preview"], True),
    "gemini-2.5-pro": (1.25, 10.00, ["gemini-2.5-pro"], True),
    "gemini-2.5-flash": (0.30, 2.50, ["gemini-2.5-flash"], True),
    "gemini-2.5-flash-lite": (0.10, 0.40, ["gemini-2.5-flash-lite"], True),
    "gemini-2.0-flash": (0.15, 0.60, ["gemini-2.0-flash"], True),
    "gemini-2.0-flash-lite": (0.075, 0.30, ["gemini-2.0-flash-lite"], True),
    "gemini-1.5-pro": (3.50, 10.50, ["gemini-1.5-pro-latest"], True),
    "gemini-1.5-flash": (0.075, 0.30, ["gemini-1.5-flash-latest", "gemini-1.5-flash-8b"], True),

    # Amazon Bedrock (ALL available text/chat models — exact modelId + on-demand pricing)
    # AI21 Labs
    "ai21.jamba-1-5-large-v1:0": (2.00, 8.00, ["jamba-1-5-large-bedrock"], False),
    "ai21.jamba-1-5-mini-v1:0": (0.20, 0.40, ["jamba-1-5-mini-bedrock"], False),

    # Anthropic on Bedrock
    "anthropic.claude-3-5-sonnet-20241022-v2:0": (6.00, 30.00, ["claude-3-5-sonnet-bedrock", "claude-3-5-sonnet-v2-bedrock"], True),
    "anthropic.claude-3-5-haiku-20241022-v1:0": (1.00, 5.00, ["claude-3-5-haiku-bedrock"], True),
    "anthropic.claude-haiku-4-5-20251001-v1:0": (1.00, 5.00, ["claude-haiku-4-5-bedrock"], True),
    "anthropic.claude-opus-4-1-20250805-v1:0": (15.00, 75.00, ["claude-opus-4-1-bedrock"], True),
    "anthropic.claude-opus-4-5-20251101-v1:0": (5.00, 25.00, ["claude-opus-4-5-bedrock"], True),
    "anthropic.claude-opus-4-6-v1": (5.00, 25.00, ["claude-opus-4-6-bedrock"], True),
    "anthropic.claude-sonnet-4-5-20250929-v1:0": (3.00, 15.00, ["claude-sonnet-4-5-bedrock"], True),
    "anthropic.claude-sonnet-4-6": (3.00, 15.00, ["claude-sonnet-4-6-bedrock"], True),

    # Meta on Bedrock
    "meta.llama3-1-70b-instruct-v1:0": (0.72, 0.72, ["llama3-1-70b-bedrock"], False),
    "meta.llama3-1-8b-instruct-v1:0": (0.22, 0.22, ["llama3-1-8b-bedrock"], False),
    "meta.llama3-2-90b-instruct-v1:0": (0.72, 0.72, ["llama3-2-90b-bedrock"], True),
    "meta.llama3-2-11b-instruct-v1:0": (0.22, 0.22, ["llama3-2-11b-bedrock"], True),
    "meta.llama3-3-70b-instruct-v1:0": (0.72, 0.72, ["llama3-3-70b-bedrock"], False),
    "meta.llama4-maverick-17b-instruct-v1:0": (0.50, 2.00, ["llama4-maverick-bedrock"], True),
    "meta.llama4-scout-17b-instruct-v1:0": (0.40, 1.60, ["llama4-scout-bedrock"], True),

    # Mistral AI on Bedrock
    "mistral.devstral-2-135b": (0.40, 2.00, ["devstral-2-135b-bedrock"], False),
    "mistral.magistral-small-1-2": (0.50, 1.50, ["magistral-small-bedrock"], False),
    "mistral.voxtral-mini-1-0": (0.04, 0.04, ["voxtral-mini-bedrock"], False),
    "mistral.voxtral-small-1-0": (0.10, 0.30, ["voxtral-small-bedrock"], False),
    "mistral.ministral-3b-3-0": (0.10, 0.10, ["ministral-3b-bedrock"], False),
    "mistral.ministral-8b-3-0": (0.15, 0.15, ["ministral-8b-bedrock"], False),
    "mistral.ministral-14b-3-0": (0.20, 0.20, ["ministral-14b-bedrock"], False),
    "mistral.mistral-large-3": (0.50, 1.50, ["mistral-large-3-bedrock"], False),

    # Amazon Nova
    "amazon.nova-pro-v1:0": (0.80, 4.00, ["nova-pro-bedrock"], True),
    "amazon.nova-premier-v1:0": (1.20, 6.00, ["nova-premier-bedrock"], True),
    "amazon.nova-lite-v1:0": (0.20, 0.80, ["nova-lite-bedrock"], True),
    "amazon.nova-micro-v1:0": (0.10, 0.40, ["nova-micro-bedrock"], True),

    # Legacy / additional
    "meta.llama-2-chat-70b": (1.95, 2.56, ["llama-2-70b-bedrock"], False),
    "meta.llama-2-chat-13b": (0.75, 1.00, ["llama-2-13b-bedrock"], False),

    # OpenAI legacy snapshots
    "gpt-4o-2024-11-20": (2.50, 10.00, ["gpt-4o-2024-11-20", "gpt-4o"], True),
    "gpt-4o-mini-2024-07-18": (0.15, 0.60, ["gpt-4o-mini"], True),
}

# Pre-built alias index: every alias and canonical id → canonical id
_BASELINE_ALIAS_INDEX: dict[str, str] = {}
for _cid, (_in, _out, _aliases, _) in _BASELINE.items():
    _BASELINE_ALIAS_INDEX[_cid] = _cid
    for _a in _aliases:
        _BASELINE_ALIAS_INDEX[_a] = _cid

# ---------------------------------------------------------------------------
# Signature verification
# ---------------------------------------------------------------------------

def _load_public_key():
    """
    Load the embedded Ed25519 public key. Returns None if the cryptography
    package is unavailable (falls back to baseline without verification).
    """
    try:
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
        return Ed25519PublicKey.from_public_bytes(_REGISTRY_PUBLIC_KEY_BYTES)
    except Exception as e:
        _log.warning("Pricing registry: could not load Ed25519 public key: %s", e)
        return None


def _verify_manifest(manifest: dict) -> bool:
    """
    Verify the Ed25519 signature on a parsed manifest dict.

    The canonical payload is all top-level keys except 'signature', serialised
    as JSON with sorted keys and no extra whitespace. The signature field must
    be a standard base64-encoded 64-byte Ed25519 signature.

    Returns True if the signature is valid, False otherwise.
    """
    import base64

    sig_b64 = manifest.get("signature", "")
    if not sig_b64:
        _log.warning("Pricing registry: manifest has no signature field")
        return False

    pub_key = _load_public_key()
    if pub_key is None:
        _log.warning(
            "Pricing registry: skipping signature verification — "
            "cryptography package unavailable"
        )
        return False

    payload = {k: v for k, v in manifest.items() if k != "signature"}
    payload_bytes = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode("utf-8")

    try:
        sig_bytes = base64.b64decode(sig_b64)
        from cryptography.exceptions import InvalidSignature
        pub_key.verify(sig_bytes, payload_bytes)
        return True
    except Exception:
        return False


# ---------------------------------------------------------------------------
# PricingRegistry
# ---------------------------------------------------------------------------

class PricingRegistry:
    """
    Fetches, verifies, caches, and serves per-model token pricing.

    Usage:
        registry = PricingRegistry()
        registry.load()                              # call once at startup
        rates = registry.lookup("gpt-4o-2024-11-20")
        if rates:
            input_cost_usd = tokens_in / 1_000_000 * rates[0]
            output_cost_usd = tokens_out / 1_000_000 * rates[1]

    Thread safety: load() must complete before any concurrent lookup() calls.
    After load(), the instance is read-only and fully thread-safe.
    """

    def __init__(self) -> None:
        # Index built from the verified manifest: model_id / alias → (in/M, out/M)
        self._manifest_index: dict[str, tuple[float, float]] = {}
        self._manifest_generated_at: Optional[str] = None
        self._loaded = False

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def load(self) -> None:
        """
        Fetch and verify the remote manifest, updating the local cache.

        Fallback chain:
          1. Network fetch + signature verification → update cache
          2. Existing local cache (any age) if network fails or sig invalid
          3. Bundled baseline (no network, no cache needed)

        This method is idempotent — calling it multiple times is safe but
        redundant. Intended to be called once at startup.
        """
        manifest = self._try_fetch_and_verify()
        if manifest is None:
            manifest = self._try_load_cache()
            if manifest is not None:
                self._build_index(manifest)
                age_int = int(self.manifest_age_seconds() or 0)
                _fire_alert("cache_hit", "", manifest_age_seconds=age_int)
        # _try_fetch_and_verify already called _build_index on success.
        self._loaded = True

    def lookup(self, model_id: str) -> Optional[tuple[float, float]]:
        """
        Return (input_per_million, output_per_million) for model_id, or None.

        Resolution:
          1. Verified manifest index (canonical ID or alias)
          2. Bundled baseline table (canonical ID or alias)

        Returns None if the model is not found in either source.
        """
        if model_id in self._manifest_index:
            return self._manifest_index[model_id]

        canonical = _BASELINE_ALIAS_INDEX.get(model_id)
        if canonical is not None:
            entry = _BASELINE[canonical]
            return entry[0], entry[1]

        return None

    def manifest_age_seconds(self) -> Optional[float]:
        """
        Return the age of the loaded manifest in seconds, or None if no
        manifest was loaded (baseline-only operation).
        """
        if self._manifest_generated_at is None:
            return None
        try:
            from datetime import datetime, timezone
            generated = datetime.fromisoformat(
                self._manifest_generated_at.replace("Z", "+00:00")
            )
            return (datetime.now(timezone.utc) - generated).total_seconds()
        except Exception:
            return None

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _try_fetch_and_verify(self) -> Optional[dict]:
        """
        Attempt to fetch the manifest from _MANIFEST_URL and verify its
        Ed25519 signature. On success, write to cache and return the manifest.
        On any failure (network, bad signature, parse error), return None.

        A signature failure is a hard error — the manifest is discarded and
        an alert is fired. It is never silently used.
        """
        cache_is_fresh = self._is_cache_fresh()
        if cache_is_fresh:
            _log.debug("Pricing registry: cache is fresh — skipping network fetch")
            return None

        try:
            import httpx
            _log.info("Pricing registry: fetching manifest from %s", _MANIFEST_URL)
            response = httpx.get(_MANIFEST_URL, timeout=15.0, follow_redirects=True)
            response.raise_for_status()
            manifest = response.json()
        except Exception as e:
            _log.warning("Pricing registry: network fetch failed: %s", e)
            _fire_alert("network_failure", str(e), manifest_age_seconds=None)
            return None

        if not isinstance(manifest, dict):
            _log.warning("Pricing registry: manifest is not a JSON object")
            _fire_alert("network_failure", "manifest is not a JSON object")
            return None

        if not _verify_manifest(manifest):
            _log.error(
                "Pricing registry: SIGNATURE VERIFICATION FAILED — "
                "manifest discarded. Using cache or baseline."
            )
            _fire_alert(
                "signature_failure",
                "Ed25519 signature verification failed for pricing manifest",
                manifest_age_seconds=None,
            )
            return None

        _log.info(
            "Pricing registry: manifest verified (generated_at=%s)",
            manifest.get("generated_at", "unknown"),
        )
        self._write_cache(manifest)
        self._build_index(manifest)
        age_int = int(self.manifest_age_seconds() or 0)
        _fire_alert("success", "", manifest_age_seconds=age_int)
        return manifest

    def _try_load_cache(self) -> Optional[dict]:
        """Load the cached manifest regardless of age. Returns None on any error."""
        if not _CACHE_PATH.exists():
            return None
        try:
            data = json.loads(_CACHE_PATH.read_text(encoding="utf-8"))
            manifest = data.get("manifest")
            if not isinstance(manifest, dict):
                return None
            _log.info(
                "Pricing registry: using cached manifest (fetched_at=%s)",
                data.get("fetched_at", "unknown"),
            )
            return manifest
        except Exception as e:
            _log.warning("Pricing registry: failed to load cache: %s", e)
            return None

    def _is_cache_fresh(self) -> bool:
        """True if the cache exists and is less than _CACHE_TTL_SECONDS old."""
        if not _CACHE_PATH.exists():
            return False
        try:
            data = json.loads(_CACHE_PATH.read_text(encoding="utf-8"))
            fetched_at = data.get("fetched_at_unix", 0)
            return (time.time() - fetched_at) < _CACHE_TTL_SECONDS
        except Exception:
            return False

    @staticmethod
    def _write_cache(manifest: dict) -> None:
        """Atomically write the verified manifest to the local cache."""
        try:
            _CACHE_PATH.parent.mkdir(parents=True, exist_ok=True)
            payload = json.dumps(
                {"fetched_at_unix": time.time(), "manifest": manifest},
                indent=2,
            )
            tmp = _CACHE_PATH.with_suffix(".json.tmp")
            tmp.write_text(payload, encoding="utf-8")
            tmp.rename(_CACHE_PATH)
            _log.debug("Pricing registry: cache written to %s", _CACHE_PATH)
        except Exception as e:
            _log.warning("Pricing registry: failed to write cache: %s", e)

    def _build_index(self, manifest: dict) -> None:
        """
        Build an in-memory lookup index from a verified manifest.

        Manifest format:
        {
          "manifest_version": "1",
          "generated_at": "<ISO 8601>",
          "models": [
            {
              "model_id": "<canonical ID>",
              "input_per_million": <float>,
              "output_per_million": <float>,
              "aliases": ["<alt id>", ...]
            },
            ...
          ],
          "signature": "<base64 Ed25519 sig>"
        }

        Both canonical model_id and all aliases are inserted into the index.
        Duplicate aliases (across entries) last-write-wins.
        """
        models = manifest.get("models", [])
        if not isinstance(models, list):
            _log.warning("Pricing registry: manifest 'models' is not a list")
            return

        self._manifest_generated_at = manifest.get("generated_at")

        for entry in models:
            if not isinstance(entry, dict):
                continue
            model_id = entry.get("model_id", "")
            try:
                in_rate = float(entry["input_per_million"])
                out_rate = float(entry["output_per_million"])
            except (KeyError, TypeError, ValueError):
                _log.debug(
                    "Pricing registry: skipping malformed entry for model_id=%r", model_id
                )
                continue

            rates = (in_rate, out_rate)
            if model_id:
                self._manifest_index[model_id] = rates
            for alias in entry.get("aliases", []):
                if isinstance(alias, str) and alias:
                    self._manifest_index[alias] = rates

        _log.info(
            "Pricing registry: index built — %d model IDs / aliases loaded",
            len(self._manifest_index),
        )


# ---------------------------------------------------------------------------
# Alert helper
# ---------------------------------------------------------------------------

def _fire_alert(
    status: str,
    detail: str,
    manifest_age_seconds: Optional[int] = None,
) -> None:
    """
    Record a pricing registry fetch event. Called with:
      status = "success" | "signature_failure" | "network_failure" | "cache_hit"
      detail = human-readable detail string (empty for success)
      manifest_age_seconds = seconds since manifest generated_at, or None

    Logs at the appropriate level, attempts the configured alert webhook for
    signature failures, and reports to ma-core for Prometheus metrics (best-effort).
    """
    if status == "signature_failure":
        _log.error("ALERT pricing_registry status=signature_failure detail=%s", detail)
        _send_webhook_alert(f"[CRITICAL] Pricing registry signature failure: {detail}")
    elif status == "network_failure":
        _log.warning("pricing_registry status=network_failure detail=%s", detail)
    else:
        _log.debug("pricing_registry status=%s", status)

    _report_status_to_ma_core(status, manifest_age_seconds)


def _report_status_to_ma_core(
    status: str, manifest_age_seconds: Optional[int]
) -> None:
    """
    Best-effort IPC call to ma-core so it can increment the Prometheus counter
    ma_pricing_registry_fetch_status and set ma_pricing_registry_age_seconds.

    Never raises — if ma-core is unreachable (e.g. during CLI cost command when
    ma-core is not running) this silently does nothing.
    """
    try:
        from ma_app.ipc.client import IPCClient, IPCError
        with IPCClient() as client:
            client.send({
                "type": "pricing_registry_status",
                "status": status,
                "manifest_age_seconds": manifest_age_seconds,
            })
    except Exception:
        pass


def _send_webhook_alert(message: str) -> None:
    """Best-effort POST to the configured alert webhook URL."""
    try:
        from ma_app.config.settings import Settings
        settings = Settings.load()
        obs = getattr(settings, "observability", None)
        url = getattr(obs, "alert_webhook_url", "") if obs else ""
        if not url:
            return
        import httpx
        httpx.post(
            url,
            json={"text": message, "source": "ma-app", "component": "pricing_registry"},
            timeout=5.0,
        )
    except Exception as e:
        _log.debug("Pricing registry: webhook alert failed: %s", e)


# ---------------------------------------------------------------------------
# Module-level singleton (loaded lazily on first access)
# ---------------------------------------------------------------------------

_registry: Optional[PricingRegistry] = None


def get_registry() -> PricingRegistry:
    """
    Return the module-level PricingRegistry singleton, loading it on first call.

    Thread safety: not safe for concurrent first-call scenarios. In practice,
    the registry is first accessed during CLI command startup (single-threaded).
    Subsequent read-only lookups are safe from any thread.
    """
    global _registry
    if _registry is None:
        _registry = PricingRegistry()
        _registry.load()
    return _registry


# ---------------------------------------------------------------------------
# AWS Bedrock Pricing API integration
# ---------------------------------------------------------------------------

def fetch_bedrock_pricing(
    model_id: str,
    region: str,
) -> Optional[tuple[float, float]]:
    """
    Fetch per-token pricing for a Bedrock-hosted model from the AWS Pricing API.

    Returns (input_per_million, output_per_million) on success, or None if the
    lookup fails for any reason (permission denied, model not found, network
    error, unexpected response shape).

    IAM requirement: pricing:GetProducts on arn:aws:pricing:*:*:*
    This permission must be in a separate IAM policy from Bedrock inference
    permissions to avoid confused-deputy privilege escalation.

    Note: the AWS Pricing API is only available via us-east-1 and ap-south-1
    regardless of which region's pricing is being queried. This function always
    contacts us-east-1.

    Normalization version: _BEDROCK_API_FORMAT_VERSION = "2026-04".
    If AWS changes the response shape, update the normalization and bump the
    version constant alongside a MA release. This function never adapts to
    unknown shapes — it returns None rather than silently computing wrong costs.
    """
    try:
        import boto3
        import botocore.exceptions
    except ImportError:
        _log.warning("fetch_bedrock_pricing: boto3 not available")
        return None

    try:
        client = boto3.client("pricing", region_name="us-east-1")
        response = client.get_products(
            ServiceCode="AmazonBedrock",
            Filters=[
                {"Type": "TERM_MATCH", "Field": "model",      "Value": model_id},
                {"Type": "TERM_MATCH", "Field": "regionCode", "Value": region},
            ],
            FormatVersion="aws_v1",
            MaxResults=10,
        )
    except Exception as e:
        _log.warning(
            "fetch_bedrock_pricing: API call failed for model=%s region=%s: %s",
            model_id,
            region,
            e,
        )
        return None

    price_list = response.get("PriceList", [])
    if not price_list:
        _log.debug(
            "fetch_bedrock_pricing: no price list entries for model=%s region=%s",
            model_id,
            region,
        )
        return None

    return _parse_bedrock_price_list(price_list, model_id, region)


def _parse_bedrock_price_list(
    price_list: list,
    model_id: str,
    region: str,
) -> Optional[tuple[float, float]]:
    """
    Parse the PriceList from a GetProducts response and extract
    (input_per_million, output_per_million).

    Normalization version: _BEDROCK_API_FORMAT_VERSION = "2026-04".

    The expected structure of each price_list item (a JSON string):
    {
      "product": {
        "attributes": {"model": "...", "regionCode": "...", ...}
      },
      "terms": {
        "OnDemand": {
          "<offer_id>": {
            "priceDimensions": {
              "<dim_id>": {
                "description": "... input tokens ...",
                "unit": "1K tokens",
                "pricePerUnit": {"USD": "0.0000030000"}
              }
            }
          }
        }
      }
    }

    The price is per 1K tokens in the API response. We convert to per-million
    by multiplying by 1000. The token type (input/output) is identified by
    scanning the description field for the keywords "input" and "output".
    """
    input_per_million: Optional[float] = None
    output_per_million: Optional[float] = None

    for item_str in price_list:
        try:
            if isinstance(item_str, str):
                item = json.loads(item_str)
            elif isinstance(item_str, dict):
                item = item_str
            else:
                continue
        except (json.JSONDecodeError, TypeError):
            continue

        terms = item.get("terms", {})
        on_demand = terms.get("OnDemand", {})
        if not isinstance(on_demand, dict):
            continue

        for offer in on_demand.values():
            if not isinstance(offer, dict):
                continue
            price_dims = offer.get("priceDimensions", {})
            if not isinstance(price_dims, dict):
                continue

            for dim in price_dims.values():
                if not isinstance(dim, dict):
                    continue
                description = (dim.get("description") or "").lower()
                price_per_unit = dim.get("pricePerUnit", {})
                usd_str = price_per_unit.get("USD", "")
                unit = (dim.get("unit") or "").lower()

                try:
                    price_per_unit_usd = float(usd_str)
                except (ValueError, TypeError):
                    continue

                # The Bedrock Pricing API (format version 2024-01) reports
                # prices per 1K tokens. Convert to per million.
                if "1k" in unit or "1,000" in unit:
                    price_per_million = price_per_unit_usd * 1_000.0
                elif "token" in unit and "1k" not in unit and "1,000" not in unit:
                    # Per-token price — multiply by 1,000,000
                    price_per_million = price_per_unit_usd * 1_000_000.0
                else:
                    _log.debug(
                        "fetch_bedrock_pricing: unrecognised unit %r for model=%s — "
                        "skipping (format version %s)",
                        unit,
                        model_id,
                        _BEDROCK_API_FORMAT_VERSION,
                    )
                    continue

                if "input" in description:
                    input_per_million = price_per_million
                elif "output" in description:
                    output_per_million = price_per_million

    if input_per_million is None or output_per_million is None:
        _log.debug(
            "fetch_bedrock_pricing: could not extract both input and output rates "
            "for model=%s region=%s (format version %s)",
            model_id,
            region,
            _BEDROCK_API_FORMAT_VERSION,
        )
        return None

    _log.info(
        "fetch_bedrock_pricing: model=%s region=%s → "
        "input=%.4f/M output=%.4f/M (format version %s)",
        model_id,
        region,
        input_per_million,
        output_per_million,
        _BEDROCK_API_FORMAT_VERSION,
    )
    return input_per_million, output_per_million