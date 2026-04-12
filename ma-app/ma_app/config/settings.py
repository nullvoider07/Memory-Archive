# /Memory-Archive/ma-app/ma_app/config/settings.py

from __future__ import annotations

import json
import os
import tempfile
from typing import Optional
from dataclasses import asdict, dataclass, field
from pathlib import Path

CONFIG_PATH = (
    Path(os.environ["MEMORY_ARCHIVE_CONFIG"])
    if "MEMORY_ARCHIVE_CONFIG" in os.environ
    else Path.home() / ".memory-archive" / "config.json"
)

# Sub-configs
@dataclass
class AwsConfig:
    bucket: str = ""
    region: str = ""

@dataclass
class AzureConfig:
    container: str = ""
    account: str = ""
    storage_type: str = "auto"

@dataclass
class GcpConfig:
    bucket: str = ""
    project: str = ""

@dataclass
class NamedBackendConfig:
    name: str = ""
    provider: str = ""
    bucket: str = ""
    region: str = ""
    account: str = ""
    container: str = ""
    project: str = ""

@dataclass
class RoutingRuleConfig:
    match_tenant_prefix: str = ""
    match_default: bool = False
    backend: str = ""

@dataclass
class CloudConfig:
    provider: str = ""
    aws: AwsConfig = field(default_factory=AwsConfig)
    azure: AzureConfig = field(default_factory=AzureConfig)
    gcp: GcpConfig = field(default_factory=GcpConfig)
    backends: list[NamedBackendConfig] = field(default_factory=list)
    routing_rules: list[RoutingRuleConfig] = field(default_factory=list)

@dataclass
class ProviderConfig:
    name: str = ""
    endpoint: str = ""
    api_key_ref: str = ""
    priority: int = 1
    cost_per_million_input_tokens: float = 0.0
    cost_per_million_output_tokens: float = 0.0

@dataclass
class ModelConfig:
    provider: str = ""
    endpoint: str = ""
    api_key_ref: str = ""
    context_window_steps: int = 5
    requests_per_minute: int = 60
    token_budget_per_hour: int = 1000000
    circuit_breaker_threshold: int = 5
    circuit_breaker_reset_seconds: int = 60
    max_retries: int = 3
    cost_per_million_input_tokens: float = 3.00
    cost_per_million_output_tokens: float = 15.00
    request_timeout_seconds: int = 30
    routing_policy: str = "pinned"
    providers: list[ProviderConfig] = field(default_factory=list)

# Main settings
@dataclass
class Settings:
    redis_url: str = "redis://127.0.0.1:6379"
    ipc_socket_path: str = str(Path.home() / ".memory-archive" / "ma.sock")
    storage_path: str = str(Path.home() / ".memory-archive" / "memories")
    storage_mode: str = "local"
    control_center_addr: str = ""
    the_eyes_addr: str = ""
    the_eyes_poll_interval_seconds: int = 10
    silence_timeout_seconds: int = 30
    kafka_broker: str = ""
    sync_retry_max: int = 5
    ma_core_addr: str = ""
    ipc_port: Optional[int] = None
    ipc_bind_addr: str = "0.0.0.0"
    ipc_server_fingerprint: str = ""
    annotator_id: str = ""
    annotator_key: str = ""
    temp_session_dir: str = str(Path(tempfile.gettempdir()) / "ma-sessions")
    metadata_flush_interval: int = 10
    tenant_id: str = ""
    cloud: CloudConfig = field(default_factory=CloudConfig)
    model: ModelConfig = field(default_factory=ModelConfig)

    # Load / save
    @classmethod
    def load(cls) -> "Settings":
        """
        Load settings from config.json.
        Returns defaults if the file does not exist.
        Raises if the file exists but cannot be parsed.
        """
        if not CONFIG_PATH.exists():
            return cls()

        raw = CONFIG_PATH.read_text(encoding="utf-8")
        data = json.loads(raw)
        return cls._from_dict(data)

    # Save the current settings to config.json, creating the directory if needed
    def save(self) -> None:
        """Write settings to config.json. Creates the directory if needed."""
        CONFIG_PATH.parent.mkdir(parents=True, exist_ok=True)
        CONFIG_PATH.write_text(
            json.dumps(self._to_dict(), indent=2),
            encoding="utf-8",
        )

    # Helpers
    def _to_dict(self) -> dict:
        return asdict(self)

    @classmethod
    def _from_dict(cls, data: dict) -> "Settings":
        cloud_data = data.get("cloud", {})
        cloud = CloudConfig(
            provider=cloud_data.get("provider", ""),
            aws=AwsConfig(**cloud_data.get("aws", {})),
            azure=AzureConfig(
                container=cloud_data.get("azure", {}).get("container", ""),
                account=cloud_data.get("azure", {}).get("account", ""),
                storage_type=cloud_data.get("azure", {}).get("storage_type", "auto"),
            ),
            gcp=GcpConfig(**cloud_data.get("gcp", {})),
            backends=[
                NamedBackendConfig(
                    name=b.get("name", ""),
                    provider=b.get("provider", ""),
                    bucket=b.get("bucket", ""),
                    region=b.get("region", ""),
                    account=b.get("account", ""),
                    container=b.get("container", ""),
                    project=b.get("project", ""),
                )
                for b in cloud_data.get("backends", [])
            ],
            routing_rules=[
                RoutingRuleConfig(
                    match_tenant_prefix=r.get("match_tenant_prefix", ""),
                    match_default=bool(r.get("match_default", False)),
                    backend=r.get("backend", ""),
                )
                for r in cloud_data.get("routing_rules", [])
            ],
        )
        model_data = data.get("model", {})

        # Backward compatibility: cost_per_1k_* → cost_per_million_*.
        # If the old field names are present and the new ones are absent,
        # convert the values (multiply by 1000) and emit a deprecation warning.
        # The new names will be written to config.json on the next save().
        _has_old_input = "cost_per_1k_input_tokens" in model_data
        _has_new_input = "cost_per_million_input_tokens" in model_data
        _has_old_output = "cost_per_1k_output_tokens" in model_data
        _has_new_output = "cost_per_million_output_tokens" in model_data

        if (_has_old_input and not _has_new_input) or (_has_old_output and not _has_new_output):
            import logging as _logging
            _logging.getLogger(__name__).warning(
                "config.json uses deprecated cost_per_1k_* fields. "
                "Values have been converted to cost_per_million_* (multiplied by 1000). "
                "Run 'memory-archive config --save' to update config.json."
            )

        if _has_new_input:
            _input_rate = float(model_data["cost_per_million_input_tokens"])
        elif _has_old_input:
            _input_rate = float(model_data["cost_per_1k_input_tokens"]) * 1000.0
        else:
            _input_rate = 3.00

        if _has_new_output:
            _output_rate = float(model_data["cost_per_million_output_tokens"])
        elif _has_old_output:
            _output_rate = float(model_data["cost_per_1k_output_tokens"]) * 1000.0
        else:
            _output_rate = 15.00

        model = ModelConfig(
            provider=model_data.get("provider", ""),
            endpoint=model_data.get("endpoint", ""),
            api_key_ref=model_data.get("api_key_ref", ""),
            context_window_steps=int(model_data.get("context_window_steps", 5)),
            requests_per_minute=int(model_data.get("requests_per_minute", 60)),
            token_budget_per_hour=int(model_data.get("token_budget_per_hour", 1000000)),
            circuit_breaker_threshold=int(model_data.get("circuit_breaker_threshold", 5)),
            circuit_breaker_reset_seconds=int(model_data.get("circuit_breaker_reset_seconds", 60)),
            max_retries=int(model_data.get("max_retries", 3)),
            cost_per_million_input_tokens=_input_rate,
            cost_per_million_output_tokens=_output_rate,
            request_timeout_seconds=int(model_data.get("request_timeout_seconds", 30)),
            routing_policy=model_data.get("routing_policy", "pinned"),
            providers=[
                ProviderConfig(
                    name=p.get("name", ""),
                    endpoint=p.get("endpoint", ""),
                    api_key_ref=p.get("api_key_ref", ""),
                    priority=int(p.get("priority", 1)),
                    cost_per_million_input_tokens=float(
                        p.get("cost_per_million_input_tokens", 0.0)
                    ),
                    cost_per_million_output_tokens=float(
                        p.get("cost_per_million_output_tokens", 0.0)
                    ),
                )
                for p in model_data.get("providers", [])
            ],
        )
        if "ipc_token" in data:
            import logging as _logging
            _logging.getLogger(__name__).warning(
                "config.json contains 'ipc_token' which is no longer used. "
                "Set the MA_IPC_TOKEN environment variable instead. "
                "The config field will be ignored."
            )
        if "annotator_key_hash" in data:
            import logging as _logging
            _logging.getLogger(__name__).warning(
                "config.json contains 'annotator_key_hash' which has been replaced "
                "by the per-annotator Redis credential registry. "
                "The config field will be ignored."
            )
        return cls(
            redis_url=data.get("redis_url", "redis://127.0.0.1:6379"),
            ipc_socket_path=data.get(
                "ipc_socket_path",
                str(Path.home() / ".memory-archive" / "ma.sock"),
            ),
            storage_path=data.get(
                "storage_path",
                str(Path.home() / ".memory-archive" / "memories"),
            ),
            storage_mode=data.get("storage_mode", "local"),
            control_center_addr=data.get("control_center_addr", ""),
            the_eyes_addr=data.get("the_eyes_addr", ""),
            the_eyes_poll_interval_seconds=int(data.get("the_eyes_poll_interval_seconds", 10)),
            silence_timeout_seconds=data.get("silence_timeout_seconds", 30),
            kafka_broker=data.get("kafka_broker", ""),
            sync_retry_max=int(data.get("sync_retry_max", 5)),
            ma_core_addr=data.get("ma_core_addr", ""),
            ipc_port=data.get("ipc_port", None),
            ipc_bind_addr=data.get("ipc_bind_addr", "0.0.0.0"),
            ipc_server_fingerprint=data.get("ipc_server_fingerprint", ""),
            annotator_id=data.get("annotator_id", ""),
            annotator_key=data.get("annotator_key", ""),
            temp_session_dir=data.get(
                "temp_session_dir",
                str(Path(tempfile.gettempdir()) / "ma-sessions"),
            ),
            metadata_flush_interval=int(data.get("metadata_flush_interval", 10)),
            tenant_id=data.get("tenant_id", ""),
            cloud=cloud,
            model=model,
        )

    # Display
    def display(self) -> str:
        """Return a human-readable summary for `memory-archive config --show`."""
        lines = [
            f"  redis_url                : {self.redis_url}",
            f"  ipc_socket_path          : {self.ipc_socket_path}",
            f"  storage_path             : {self.storage_path}",
            f"  storage_mode             : {self.storage_mode}",
            f"  control_center_addr      : {self.control_center_addr or '(not set)'}",
            f"  the_eyes_addr            : {self.the_eyes_addr or '(not set)'}",
            f"  the_eyes_poll_interval_s  : {self.the_eyes_poll_interval_seconds}",
            f"  silence_timeout_seconds  : {self.silence_timeout_seconds}",
            f"  kafka_broker             : {self.kafka_broker or '(not set)'}",
            f"  ma_core_addr             : {self.ma_core_addr or '(not set)'}",
            f"  ipc_port                 : {self.ipc_port or '(not set)'}",
            f"  ipc_bind_addr            : {self.ipc_bind_addr}",
            f"  MA_IPC_TOKEN             : {'(set)' if __import__('os').environ.get('MA_IPC_TOKEN') else '(not set — required when ipc_port is configured)'}",
            f"  ipc_server_fingerprint   : {self.ipc_server_fingerprint or '(not set)'}",
            f"  annotator_id             : {self.annotator_id or '(not set)'}",
            f"  annotator_key            : {'(set)' if self.annotator_key else '(not set)'}",
            f"  temp_session_dir         : {self.temp_session_dir}",
            f"  metadata_flush_interval  : {self.metadata_flush_interval}",
            ]
        if self.cloud.backends:
            lines.append(f"  cloud.backends           : {len(self.cloud.backends)} configured")
            for b in self.cloud.backends:
                lines.append(f"    [{b.name}] provider={b.provider}")
                if b.provider == "aws":
                    lines.append(f"      bucket={b.bucket}  region={b.region}")
                elif b.provider == "azure":
                    lines.append(f"      account={b.account}  container={b.container}")
                elif b.provider == "gcp":
                    lines.append(f"      bucket={b.bucket}  project={b.project}")
            if self.cloud.routing_rules:
                lines.append("  cloud.routing_rules      :")
                for r in self.cloud.routing_rules:
                    if r.match_default:
                        lines.append(f"    (default) → {r.backend}")
                    elif r.match_tenant_prefix:
                        lines.append(f"    tenant_prefix={r.match_tenant_prefix!r} → {r.backend}")
        else:
            lines.append(f"  cloud.provider           : {self.cloud.provider or '(not set)'}")
            if self.cloud.provider == "aws":
                lines += [
                    f"  cloud.aws.bucket         : {self.cloud.aws.bucket}",
                    f"  cloud.aws.region         : {self.cloud.aws.region}",
                ]
            elif self.cloud.provider == "azure":
                lines += [
                    f"  cloud.azure.container    : {self.cloud.azure.container}",
                    f"  cloud.azure.account      : {self.cloud.azure.account}",
                    f"  cloud.azure.storage_type : {self.cloud.azure.storage_type}",
                ]
            elif self.cloud.provider == "gcp":
                lines += [
                    f"  cloud.gcp.bucket         : {self.cloud.gcp.bucket}",
                    f"  cloud.gcp.project        : {self.cloud.gcp.project}",
                ]
        lines.append(f"  tenant_id                : {self.tenant_id or '(not set)'}")
        if self.model.providers:
            lines.append(f"  model.routing_policy     : {self.model.routing_policy}")
            lines.append(f"  model.providers          : {len(self.model.providers)} configured")
            for p in sorted(self.model.providers, key=lambda x: x.priority):
                lines.append(f"    [{p.name}] priority={p.priority}  endpoint={p.endpoint or '(not set)'}  api_key_ref={'(set)' if p.api_key_ref else '(not set)'}")
        else:
            lines += [
                f"  model.provider           : {self.model.provider or '(not set)'}",
                f"  model.endpoint           : {self.model.endpoint or '(not set)'}",
                f"  model.api_key_ref        : {'(set)' if self.model.api_key_ref else '(not set)'}",
            ]
        lines += [
            f"  model.context_window     : {self.model.context_window_steps}",
            f"  model.requests_per_min   : {self.model.requests_per_minute}",
            f"  model.circuit_breaker    : threshold={self.model.circuit_breaker_threshold} reset={self.model.circuit_breaker_reset_seconds}s",
            f"  model.request_timeout    : {self.model.request_timeout_seconds}s",
            f"  model.input_rate         : ${self.model.cost_per_million_input_tokens:.4f}/M tokens (config fallback)",
            f"  model.output_rate        : ${self.model.cost_per_million_output_tokens:.4f}/M tokens (config fallback)",
        ]
        return "\n".join(lines)