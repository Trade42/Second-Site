"""Interior (Matterport) and exterior (Vexcel) enrichment of a PropertyDigitalProfile.

Truthfulness rules applied here:
- A Matterport scan counts as the listed space only when its title names the listed suite. Other scans in the
  building are kept as context (`matches_listing_unit=False`) and never drive layouts.
- Vexcel element values are model detections from aerial imagery: VISUALLY_OBSERVED with their scores as
  limitations. They describe the building/parcel, not the suite.
Provider failures degrade to `missing_information`; they never fail the search.
"""

from __future__ import annotations

import logging
from datetime import UTC, date, datetime
from typing import Any

from app.contracts import (
    DataOrigin,
    EvidenceClassification,
    ExteriorAsset,
    InfrastructureObservation,
    PropertyDigitalProfile,
    SourceManifestEntry,
)
from app.integrations.base import ProviderError
from app.integrations.matterport import suite_matches

log = logging.getLogger(__name__)
MAX_MODELS = 40


def _manifest(prop: PropertyDigitalProfile, entry: SourceManifestEntry) -> list[SourceManifestEntry]:
    return [m for m in prop.source_manifest if m.provider != entry.provider] + [entry]


def enrich_interior(prop: PropertyDigitalProfile, matterport: Any) -> PropertyDigitalProfile:
    mp = [a for a in prop.interior_assets if a.provider == "matterport"]
    if not mp or getattr(matterport, "mode", "fixture") != "live":
        return prop
    others = [a for a in prop.interior_assets if a.provider != "matterport"]
    missing = list(prop.missing_information)
    try:
        summaries = matterport.model_summaries([a.asset_id for a in mp][:MAX_MODELS])
    except ProviderError as exc:
        log.warning("matterport enrichment failed: %s", exc)
        missing.append(f"Matterport scans not retrieved ({exc.kind})")
        return prop.model_copy(update={"missing_information": missing})
    suite = prop.property_identity.unit_or_suite
    listed_area = prop.commercial_listing.available_area_sqm if prop.commercial_listing else None
    enriched = []
    for a in mp:
        s = summaries.get(a.asset_id)
        if s is None:
            continue  # not accessible with this token
        match = suite_matches(s.get("name"), suite)
        enriched.append(
            a.model_copy(
                update={
                    "label": s.get("name"),
                    "area_sqm": s.get("area_sqm") or None,
                    "matches_listing_unit": match,
                    "has_measured_geometry": bool(match and s.get("floor_count")),
                    "data_origin": DataOrigin.LIVE,
                }
            )
        )
    enriched.sort(
        key=lambda a: (
            not a.matches_listing_unit,
            abs((a.area_sqm or 0) - (listed_area or 0)) if listed_area else 0,
        )
    )
    matched = [a for a in enriched if a.matches_listing_unit]
    missing = [m for m in missing if not m.startswith("No interior model")]
    if not matched:
        missing.append(
            f"No Matterport scan of suite {suite or '(unspecified)'}; {len(enriched)} scan(s) of other spaces in the building"
        )
    note = (
        f"{len(matched)} scan(s) of the listed suite"
        if matched
        else "building scans only; listed suite not scanned"
    )
    return prop.model_copy(
        update={
            "interior_assets": matched + [a for a in enriched if not a.matches_listing_unit][:6] + others,
            "missing_information": missing,
            "source_manifest": _manifest(
                prop,
                SourceManifestEntry(
                    provider="matterport",
                    dataset="model_api",
                    status="LIVE",
                    record_count=len(enriched),
                    retrieved_at=datetime.now(UTC),
                    note=note,
                ),
            ),
        }
    )


def _obs(prop_id, typ, value, unit, desc, limitations) -> InfrastructureObservation:
    return InfrastructureObservation(
        property_id=prop_id,
        observation_type=typ,
        value=value,
        unit=unit,
        classification=EvidenceClassification.VISUALLY_OBSERVED,
        description=desc,
        limitations=limitations,
    )


def enrich_exterior(prop: PropertyDigitalProfile, vexcel: Any, media_base: str) -> PropertyDigitalProfile:
    ident = prop.property_identity
    if getattr(vexcel, "mode", "fixture") != "live" or ident.latitude is None or ident.longitude is None:
        return prop
    missing = [m for m in prop.missing_information if m != "No exterior imagery"]
    try:
        el = vexcel.property_elements(ident.latitude, ident.longitude)
    except ProviderError as exc:
        log.warning("vexcel enrichment failed: %s", exc)
        return prop.model_copy(
            update={"missing_information": [*missing, f"Vexcel imagery not retrieved ({exc.kind})"]}
        )
    img_date = None
    raw_date = el.get("image_date") if el else None
    if not raw_date:
        try:  # no structure elements here: fall back to the latest urban ortho capture covering the point
            urban = [
                c for c in vexcel.capture_dates(ident.latitude, ident.longitude) if c.get("layer") == "urban"
            ]
            raw_date = max((c.get("last-capture-date") or "" for c in urban), default=None) or None
        except ProviderError:
            raw_date = None
    if raw_date:
        try:
            img_date = date.fromisoformat(str(raw_date)[:10])
        except ValueError:
            img_date = None
    asset = ExteriorAsset(
        provider="vexcel",
        asset_id=f"ortho:{ident.latitude:.6f},{ident.longitude:.6f}",
        asset_type="orthomosaic",
        data_origin=DataOrigin.LIVE,
        captured_at=img_date,
        url=f"{media_base}/api/v1/media/vexcel/ortho/{prop.property_id}",
    )
    obs: list[InfrastructureObservation] = []
    if el:
        lim = ["Automated detection from aerial imagery; describes the building, not the suite."]
        dist = el.get("structure_distance_m")
        if dist and dist > 3:
            lim.append(
                f"Nearest structure is {dist:.0f} m from the geocoded point; it may be a neighbouring building."
            )
        src = f"Vexcel {el.get('collection') or ''} {str(el.get('image_date') or '')[:10]}".strip()
        if el.get("footprint_area_sqm"):
            obs.append(
                _obs(
                    prop.property_id,
                    "building_footprint_area",
                    round(el["footprint_area_sqm"], 1),
                    "m2",
                    f"Building footprint from {src}",
                    lim,
                )
            )
        if el.get("roof_condition") is not None:
            obs.append(
                _obs(
                    prop.property_id,
                    "roof_condition",
                    el["roof_condition"],
                    "score_1_5",
                    f"Roof condition score (1 poor - 5 excellent) from {src}",
                    lim,
                )
            )
        if el.get("roof_material"):
            obs.append(
                _obs(
                    prop.property_id,
                    "roof_material",
                    el["roof_material"],
                    None,
                    f"Roof material (detector score {el.get('roof_material_score')})",
                    lim,
                )
            )
        if el.get("rooftop_ac_units") is not None:
            obs.append(
                _obs(
                    prop.property_id,
                    "rooftop_hvac_units",
                    el["rooftop_ac_units"],
                    "count",
                    "Rooftop HVAC/AC units detected (existing equipment, not spare capacity)",
                    lim,
                )
            )
        if el.get("structural_damage_detected") or el.get("water_pooling_detected"):
            obs.append(
                _obs(
                    prop.property_id,
                    "roof_defects",
                    "; ".join(
                        k
                        for k in (
                            "structural damage" if el.get("structural_damage_detected") else "",
                            "water pooling" if el.get("water_pooling_detected") else "",
                        )
                        if k
                    ),
                    None,
                    "Roof defects flagged by imagery model; ask landlord about roof repairs",
                    lim,
                )
            )
    return prop.model_copy(
        update={
            "exterior_assets": [a for a in prop.exterior_assets if a.provider != "vexcel"] + [asset],
            "building_observations": [
                o
                for o in prop.building_observations
                if not o.description.startswith(("Building footprint from Vexcel", "Roof", "Rooftop"))
            ]
            + obs,
            "missing_information": missing,
            "source_manifest": _manifest(
                prop,
                SourceManifestEntry(
                    provider="vexcel",
                    dataset="ortho+property",
                    status="LIVE",
                    record_count=1 + len(obs),
                    retrieved_at=datetime.now(UTC),
                    note=f"Imagery {img_date or 'date unknown'}",
                ),
            ),
        }
    )
