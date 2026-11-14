"""Validated configuration. Secrets are never printed; see `Settings.redacted_summary()`."""

from __future__ import annotations

from enum import StrEnum
from functools import lru_cache
from pathlib import Path

from pydantic import Field, SecretStr
from pydantic_settings import BaseSettings, SettingsConfigDict


class ProviderMode(StrEnum):
    LIVE = "live"
    FIXTURE = "fixture"
    DISABLED = "disabled"


REPO_ROOT = Path(__file__).resolve().parents[4]


class Settings(BaseSettings):
    model_config = SettingsConfigDict(env_file=(".env", "../../.env"), extra="ignore")

    # APPLICATION
    app_env: str = Field(default="development", alias="APP_ENV")
    app_version: str = "0.1.0"
    log_level: str = Field(default="INFO", alias="LOG_LEVEL")
    cors_origins: str = Field(default="http://localhost:3000", alias="CORS_ORIGINS")
    public_base_url: str = Field(default="http://localhost:8000", alias="PUBLIC_BASE_URL")
    public_web_url: str = Field(default="http://localhost:3000", alias="PUBLIC_WEB_URL")
    task_execution_mode: str = Field(default="inline", alias="TASK_EXECUTION_MODE")  # inline | redis

    # DATABASE / REDIS
    database_url: str = Field(
        default="postgresql+psycopg://secondsite:secondsite@localhost:5432/secondsite", alias="DATABASE_URL"
    )
    redis_url: str = Field(default="redis://localhost:6379/0", alias="REDIS_URL")

    # AUTHENTICATION
    auth_secret: SecretStr = Field(default=SecretStr("dev-only-change-me"), alias="AUTH_SECRET")
    allow_dev_tokens: bool = Field(default=True, alias="ALLOW_DEV_TOKENS")

    # MODEL PROVIDER (LangChain init_chat_model string, e.g. "anthropic:claude-sonnet-5-5")
    supervisor_model: str | None = Field(default=None, alias="SUPERVISOR_MODEL")
    anthropic_api_key: SecretStr | None = Field(default=None, alias="ANTHROPIC_API_KEY")
    agent_checkpointer: str = Field(default="memory", alias="AGENT_CHECKPOINTER")  # memory | postgres
    # Origin of the Rust API (e.g. http://localhost:8000, no /api/v1). Unset: supervisor tools run in-process.
    backend_api_url: str | None = Field(default=None, alias="BACKEND_API_URL")

    # PROVIDER MODES
    property_provider_mode: ProviderMode = Field(default=ProviderMode.FIXTURE, alias="PROPERTY_PROVIDER_MODE")
    permit_provider_mode: ProviderMode = Field(default=ProviderMode.FIXTURE, alias="PERMIT_PROVIDER_MODE")
    voice_provider_mode: ProviderMode = Field(default=ProviderMode.FIXTURE, alias="VOICE_PROVIDER_MODE")
    messaging_provider_mode: ProviderMode = Field(
        default=ProviderMode.FIXTURE, alias="MESSAGING_PROVIDER_MODE"
    )
    procurement_provider_mode: ProviderMode = Field(
        default=ProviderMode.FIXTURE, alias="PROCUREMENT_PROVIDER_MODE"
    )

    # COSTAR: the sponsor data kit is a password-protected download (JSONL + docs), NOT an API.
    # PROPERTY_PROVIDER_MODE=live reads the extracted kit from COSTAR_KIT_DIR. Never commit the kit.
    costar_kit_dir: str = Field(default=str(REPO_ROOT / ".data" / "costar-kit"), alias="COSTAR_KIT_DIR")

    # MATTERPORT Model API (GraphQL, HTTP Basic: token id = MATTERPORT_API_KEY, secret = MATTERPORT_API_TOKEN)
    matterport_provider_mode: ProviderMode = Field(
        default=ProviderMode.FIXTURE, alias="MATTERPORT_PROVIDER_MODE"
    )
    matterport_api_key: SecretStr | None = Field(default=None, alias="MATTERPORT_API_KEY")
    matterport_api_token: SecretStr | None = Field(default=None, alias="MATTERPORT_API_TOKEN")
    matterport_sdk_key: SecretStr | None = Field(default=None, alias="MATTERPORT_SDK_KEY")
    matterport_graph_url: str = Field(
        default="https://api.matterport.com/api/models/graph", alias="MATTERPORT_GRAPH_URL"
    )

    # VEXCEL API 2.0 (X-API-KEY header)
    vexcel_provider_mode: ProviderMode = Field(default=ProviderMode.FIXTURE, alias="VEXCEL_PROVIDER_MODE")
    vexcel_api_key: SecretStr | None = Field(default=None, alias="VEXCEL_API_KEY")
    vexcel_base_url: str = Field(default="https://api.vexcelgroup.com/v2", alias="VEXCEL_BASE_URL")

    # LA OPEN DATA (Socrata). App token optional; raises rate limits.
    la_open_data_base_url: str = Field(default="https://data.lacity.org", alias="LA_OPEN_DATA_BASE_URL")
    la_open_data_app_token: SecretStr | None = Field(default=None, alias="LA_OPEN_DATA_APP_TOKEN")
    la_electrical_dataset: str = Field(default="ysqd-apz7", alias="LA_ELECTRICAL_DATASET")
    la_mechanical_dataset: str = Field(default="67is-svtd", alias="LA_MECHANICAL_DATASET")

    # PHOTON (2.0 beta: https://api.photon.codes/v1)
    photon_api_key: SecretStr | None = Field(default=None, alias="PHOTON_API_KEY")
    photon_project_id: str | None = Field(default=None, alias="PHOTON_PROJECT_ID")
    photon_webhook_secret: SecretStr | None = Field(default=None, alias="PHOTON_WEBHOOK_SECRET")
    photon_agent_handle: str | None = Field(default=None, alias="PHOTON_AGENT_HANDLE")

    # TELNYX
    telnyx_api_key: SecretStr | None = Field(default=None, alias="TELNYX_API_KEY")
    telnyx_public_key: str | None = Field(default=None, alias="TELNYX_PUBLIC_KEY")
    telnyx_connection_id: str | None = Field(default=None, alias="TELNYX_CONNECTION_ID")
    telnyx_from_number: str | None = Field(default=None, alias="TELNYX_FROM_NUMBER")

    # XAI VOICE
    xai_api_key: SecretStr | None = Field(default=None, alias="XAI_API_KEY")

    # OBJECT STORAGE
    object_storage_mode: str = Field(default="local", alias="OBJECT_STORAGE_MODE")
    object_storage_local_dir: str = Field(default="./.data/objects", alias="OBJECT_STORAGE_LOCAL_DIR")

    # SAFETY: real outbound actions are OFF unless explicitly enabled.
    allow_live_outbound_calls: bool = Field(default=False, alias="ALLOW_LIVE_OUTBOUND_CALLS")
    allow_live_purchases: bool = Field(default=False, alias="ALLOW_LIVE_PURCHASES")

    @property
    def cors_origin_list(self) -> list[str]:
        return [o.strip() for o in self.cors_origins.split(",") if o.strip()]

    def missing_for_live(self) -> dict[str, list[str]]:
        """Which live-mode providers lack credentials (names only, never values)."""
        req = {
            "matterport": (self.matterport_provider_mode, ["MATTERPORT_API_KEY", "MATTERPORT_API_TOKEN"]),
            "vexcel": (self.vexcel_provider_mode, ["VEXCEL_API_KEY"]),
            "messaging": (
                self.messaging_provider_mode,
                ["PHOTON_API_KEY", "PHOTON_PROJECT_ID", "PHOTON_WEBHOOK_SECRET"],
            ),
            "voice": (self.voice_provider_mode, ["TELNYX_API_KEY", "TELNYX_PUBLIC_KEY", "XAI_API_KEY"]),
        }
        out: dict[str, list[str]] = {}
        for name, (mode, envs) in req.items():
            if mode == ProviderMode.LIVE:
                missing = [e for e in envs if not self._has(e)]
                if missing:
                    out[name] = missing
        return out

    def _has(self, env_name: str) -> bool:
        for field_name, field in type(self).model_fields.items():
            if field.alias == env_name:
                val = getattr(self, field_name)
                if isinstance(val, SecretStr):
                    return bool(val.get_secret_value())
                return bool(val)
        return False


@lru_cache
def get_settings() -> Settings:
    return Settings()
