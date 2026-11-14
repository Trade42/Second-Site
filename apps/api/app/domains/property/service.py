"""Property Scout + digital-profile assembly. Producer of PropertyIdentity/PropertyDigitalProfile."""

from __future__ import annotations

from datetime import UTC, datetime
from uuid import UUID

from app.contracts import (
    CommercialListing,
    ExteriorAsset,
    InteriorAsset,
    PermitRecord,
    PropertyDigitalProfile,
    PropertySearchRequest,
    SourceManifestEntry,
)
from app.integrations.costar import CoStarProvider, RawListing

from .identity import build_identity

LISTING_FIELDS = set(CommercialListing.model_fields)


def profile_from_listing(project_id: UUID, raw: RawListing) -> PropertyDigitalProfile:
    a = raw.address
    identity = build_identity(
        a["street_address"],
        a.get("unit_or_suite"),
        a["city"],
        a["state"],
        a.get("postal_code"),
        a.get("parcel_id"),
        a.get("latitude"),
        a.get("longitude"),
        {raw.provider: raw.provider_listing_id},
    )
    listing = CommercialListing(
        **{k: v for k, v in raw.listing.items() if k in LISTING_FIELDS}, data_origin=raw.data_origin
    )
    interior = [
        InteriorAsset(
            provider=x["provider"],
            asset_id=x["asset_id"],
            asset_type=x["asset_type"],
            data_origin=raw.data_origin,
            has_measured_geometry=x.get("has_measured_geometry", False),
        )
        for x in raw.interior_assets
    ]
    exterior = [
        ExteriorAsset(
            provider=x["provider"],
            asset_id=x["asset_id"],
            asset_type=x["asset_type"],
            data_origin=raw.data_origin,
        )
        for x in raw.exterior_assets
    ]
    missing = []
    if not interior:
        missing.append("No interior model or measured floor plan")
    if not exterior:
        missing.append("No exterior imagery")
    if listing.existing_buildout is None:
        missing.append("Existing build-out unknown")
    now = datetime.now(UTC)
    return PropertyDigitalProfile(
        property_id=identity.property_id,
        project_id=project_id,
        property_identity=identity,
        commercial_listing=listing,
        interior_assets=interior,
        exterior_assets=exterior,
        missing_information=missing,
        source_manifest=[
            SourceManifestEntry(
                provider=raw.provider,
                dataset="listings",
                status="MOCK" if raw.provider == "fixture" else "LIVE",
                record_count=1,
                retrieved_at=now,
                note=listing.disclaimer,
            )
        ],
    )


class PropertyScoutService:
    def __init__(self, provider: CoStarProvider):
        self.provider = provider

    def search(self, project_id: UUID, request: PropertySearchRequest) -> list[PropertyDigitalProfile]:
        return [profile_from_listing(project_id, r) for r in self.provider.search(request)]


def attach_permits(
    prop: PropertyDigitalProfile, permits: list[PermitRecord], datasets: list[str], origin: str
) -> PropertyDigitalProfile:
    manifest = [m for m in prop.source_manifest if m.provider != "la_open_data"]
    manifest.append(
        SourceManifestEntry(
            provider="la_open_data",
            dataset=",".join(datasets) or None,
            status="LIVE" if origin == "LIVE" else "MOCK",
            record_count=len(permits),
            retrieved_at=datetime.now(UTC),
            note="LADBS permits 2020-present; pre-2020 history not covered",
        )
    )
    missing = [m for m in prop.missing_information if not m.startswith("No permit")]
    if not permits:
        missing.append("No permit records matched")
    return prop.model_copy(
        update={"permit_records": permits, "source_manifest": manifest, "missing_information": missing}
    )
