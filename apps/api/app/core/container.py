"""Composition root: picks fixture vs live providers from settings. Tests override via `set_container`."""

from __future__ import annotations

from dataclasses import dataclass, field

from app.core.config import ProviderMode, Settings, get_settings
from app.integrations.catalog import FixtureCatalogProvider
from app.integrations.costar import FixtureCoStarProvider, KitCoStarProvider
from app.integrations.la_permits import FixtureLAPermitProvider, LiveLAPermitProvider
from app.integrations.matterport import FixtureMatterportProvider, LiveMatterportProvider
from app.integrations.photon import FixturePhotonProvider, LivePhotonProvider
from app.integrations.storage import LocalObjectStorage
from app.integrations.telnyx import FixtureTelephonyProvider, LiveTelnyxProvider
from app.integrations.vexcel import FixtureVexcelProvider, LiveVexcelProvider
from app.services.approvals import ApprovalService
from app.services.authorization import AuthorizationService
from app.services.events import EventService
from app.services.evidence import EvidenceService
from app.services.tasks import TaskService


def _secret(s) -> str | None:
    return s.get_secret_value() if s else None


@dataclass
class Container:
    settings: Settings
    costar: object
    permits: object
    matterport: object
    vexcel: object
    photon: object
    telephony: object
    catalog: object
    storage: object
    events: EventService = field(init=False)
    tasks: TaskService = field(init=False)
    approvals: ApprovalService = field(init=False)
    authz: AuthorizationService = field(init=False)
    evidence: EvidenceService = field(init=False)

    def __post_init__(self):
        enqueue = None
        if self.settings.task_execution_mode == "redis":
            from app.workers.queue import enqueue_event

            enqueue = enqueue_event
        self.events = EventService(self.settings.task_execution_mode, enqueue)
        self.tasks = TaskService(self.events)
        self.approvals = ApprovalService(self.events)
        self.authz = AuthorizationService()
        self.evidence = EvidenceService()


def build_container(settings: Settings | None = None) -> Container:
    s = settings or get_settings()
    costar = (
        KitCoStarProvider(s.costar_kit_dir)
        if s.property_provider_mode == ProviderMode.LIVE
        else FixtureCoStarProvider()
    )
    permits = (
        LiveLAPermitProvider(
            s.la_open_data_base_url,
            s.la_electrical_dataset,
            s.la_mechanical_dataset,
            _secret(s.la_open_data_app_token),
        )
        if s.permit_provider_mode == ProviderMode.LIVE
        else FixtureLAPermitProvider()
    )
    photon = (
        LivePhotonProvider(_secret(s.photon_api_key) or "", s.photon_project_id or "")
        if s.messaging_provider_mode == ProviderMode.LIVE and s.photon_api_key
        else FixturePhotonProvider()
    )
    telephony = (
        LiveTelnyxProvider(
            _secret(s.telnyx_api_key) or "", s.telnyx_connection_id or "", s.allow_live_outbound_calls
        )
        if s.voice_provider_mode == ProviderMode.LIVE and s.telnyx_api_key
        else FixtureTelephonyProvider()
    )
    matterport = (
        LiveMatterportProvider(
            s.matterport_graph_url, _secret(s.matterport_api_key), _secret(s.matterport_api_token)
        )
        if s.matterport_provider_mode == ProviderMode.LIVE
        else FixtureMatterportProvider()
    )
    vexcel = (
        LiveVexcelProvider(s.vexcel_base_url, _secret(s.vexcel_api_key))
        if s.vexcel_provider_mode == ProviderMode.LIVE
        else FixtureVexcelProvider()
    )
    return Container(
        settings=s,
        costar=costar,
        permits=permits,
        matterport=matterport,
        vexcel=vexcel,
        photon=photon,
        telephony=telephony,
        catalog=FixtureCatalogProvider(),
        storage=LocalObjectStorage(s.object_storage_local_dir),
    )


_container: Container | None = None


def get_container() -> Container:
    global _container
    if _container is None:
        _container = build_container()
    return _container


def set_container(c: Container | None) -> None:
    global _container
    _container = c
