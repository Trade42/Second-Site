"""CoStar sponsor data.

The CoStar hackathon toolkit is a password-protected DATA KIT (JSON Lines + docs + JSON Schemas), not an API.
`KitCoStarProvider` reads the extracted kit from `COSTAR_KIT_DIR` (default `.data/costar-kit`, gitignored).
Records are genuine CoStar records delivered as a snapshot, so they are labeled
`provider="costar"`, `data_origin=FIXTURE_REAL` and source status `SNAPSHOT` — never "live API".

Field meanings follow the kit's own docs (models/commercial-listing-summary.md):
- rates are per square foot; `rateBasis` 1 = monthly, 2 = yearly; a rate of 0 means "no rate recorded";
- `serviceType` says which operating expenses the rent includes (TripleNet, FullService, ...);
- spaces with `dateOffMarket` or `isLeasePending` are not available.
`FixtureCoStarProvider` keeps the synthetic demo listings (provider="fixture") for CI and offline work.
"""

from __future__ import annotations

import csv
import json
import re
from collections import defaultdict
from dataclasses import dataclass, field
from datetime import date, datetime
from decimal import ROUND_HALF_UP, Decimal
from pathlib import Path
from typing import Any, Protocol

from app.contracts import DataOrigin, IntegrationStatus, PropertySearchRequest, ProviderStatusEntry
from app.integrations.base import FIXTURES_DIR, ProviderError, ProviderErrorKind, status_entry

SQFT_TO_SQM = 0.09290304


@dataclass(frozen=True)
class RawListing:
    provider: str
    provider_listing_id: str
    data_origin: DataOrigin
    address: dict[str, Any]
    listing: dict[str, Any]
    interior_assets: list[dict[str, Any]] = field(default_factory=list)
    exterior_assets: list[dict[str, Any]] = field(default_factory=list)


class CoStarProvider(Protocol):
    mode: str

    def search(self, request: PropertySearchRequest) -> list[RawListing]: ...

    def status(self) -> ProviderStatusEntry: ...


class FixtureCoStarProvider:
    """Synthetic listings for real LA addresses. provider is 'fixture', never 'costar'."""

    mode = "fixture"
    SUPPORTED_MARKETS = {("LOS ANGELES", "CA"), ("HOUSTON", "TX")}

    def __init__(self, path=None):
        self.path = path or FIXTURES_DIR / "properties" / "la_candidates.json"

    def search(self, request: PropertySearchRequest) -> list[RawListing]:
        if (request.city.upper(), request.state.upper()) not in self.SUPPORTED_MARKETS:
            return []
        doc = json.loads(self.path.read_text())
        out = []
        for p in doc["properties"]:
            if p["city"].upper() != request.city.upper():
                continue  # Houston is supported by the adapter but has no fixture listings yet.
            lst = p["listing"]
            if request.max_monthly_rent is not None and lst["asking_rent_monthly"] > request.max_monthly_rent:
                continue
            if request.min_area_sqm is not None and lst["available_area_sqm"] < request.min_area_sqm:
                continue
            address = {
                k: p[k]
                for k in (
                    "street_address",
                    "unit_or_suite",
                    "city",
                    "state",
                    "postal_code",
                    "parcel_id",
                    "latitude",
                    "longitude",
                )
            }
            out.append(
                RawListing(
                    "fixture",
                    lst["provider_listing_id"],
                    DataOrigin.FIXTURE_SYNTHETIC,
                    address,
                    lst,
                    p.get("interior_assets", []),
                    p.get("exterior_assets", []),
                )
            )
        return out[: request.limit]

    def status(self):
        return status_entry(
            "costar",
            self.mode,
            IntegrationStatus.MOCK,
            False,
            "Synthetic listings (provider='fixture'). Set PROPERTY_PROVIDER_MODE=live to use the CoStar kit.",
        )


# ---- CoStar data kit ---------------------------------------------------------------------------

LISTING_ACTIVE = 1
LEASE_LISTING_TYPES = {14, 16, 17, 19, 20, 21, 28}  # direct, building-park, shopping-center, sublease, ...
MATTERPORT_REF_TYPES = {36, 234}  # MatterportScan, MatterportStreamingKey
MARKET_BY_STATE = {"CA": ("LOS ANGELES", 20), "TX": ("HOUSTON", 14)}
# Space uses that can host a personal-services studio, best fit first (SpaceUse enum in the kit).
USE_PREFERENCE = {5: 0, 2: 1, 4: 1, 3: 2, 1: 3, 6: 4}
NET_SERVICE_TYPES = {"TripleNet", "Net", "DoubleNet"}  # tenant pays operating expenses on top of rent
BUILDOUT_PATTERNS: list[tuple[str, re.Pattern[str]]] = [
    ("second_generation_salon", re.compile(r"\b(salon|barber|nail|esthetic|med\s*spa|day\s*spa)\b", re.I)),
    ("open_shell", re.compile(r"\b(cold|warm|vanilla)?\s*shell\b", re.I)),
    (
        "office_buildout",
        re.compile(r"\b(built[\s-]?out|2nd\s+generation|second\s+generation|turn[\s-]?key)\b", re.I),
    ),
]


def _cents(dollars: float | Decimal) -> int:
    return int((Decimal(str(dollars)) * 100).quantize(Decimal("1"), rounding=ROUND_HALF_UP))


def _street(a: dict[str, Any]) -> str | None:
    parts = [
        a.get("streetNumber"),
        a.get("streetDirection"),
        a.get("streetName"),
        a.get("streetType"),
        a.get("streetSuffix"),
    ]
    s = " ".join(p.strip() for p in parts if p and p.strip())
    return s or None


def _date(v: str | None) -> date | None:
    if not v:
        return None
    try:
        return datetime.fromisoformat(v).date()
    except ValueError:
        return None


def _photo(uri: str, size: str = "113") -> str:
    return uri.replace("/112/", f"/{size}/")


@dataclass
class KitSpace:
    """One available lease space, flattened from a commercial listing summary."""

    listing_id: int
    space_id: int
    property_id: int
    address: dict[str, Any]
    listing: dict[str, Any]
    use_rank: int
    area_sqm: float
    rent_monthly: int | None
    photo_urls: list[str]
    floor_plan_url: str | None
    sqft: float = 0.0
    cam_per_sf_year: float | None = None


class KitCoStarProvider:
    """Searches available lease spaces in the extracted CoStar data kit (Los Angeles and Houston)."""

    mode = "live"
    LISTINGS = "data/commercial-listing-summary.jsonl"
    MODELS_CSV = "commercial-properties.csv"
    ENUMS = "schemas/enums.json"

    def __init__(self, kit_dir: str | Path):
        self.kit_dir = Path(kit_dir)
        self._spaces: list[KitSpace] | None = None
        self._models: dict[int, list[str]] | None = None
        self.skipped_no_rate = 0

    # -- loading ------------------------------------------------------------------------------
    def available(self) -> bool:
        return (self.kit_dir / self.LISTINGS).is_file()

    def _require(self) -> None:
        if not self.available():
            raise ProviderError(
                "costar",
                ProviderErrorKind.NOT_CONFIGURED,
                f"CoStar data kit not found at {self.kit_dir} (set COSTAR_KIT_DIR; see docs/PROVIDER_STATUS.md).",
            )

    def matterport_models(self) -> dict[int, list[str]]:
        """PropertyId -> Matterport model ids, from the kit's starter CSV (one row per key and model)."""
        if self._models is None:
            self._require()
            models: dict[int, list[str]] = defaultdict(list)
            with (self.kit_dir / self.MODELS_CSV).open(newline="") as f:
                for row in csv.DictReader(f):
                    mid = (row.get("MatterportModelId") or "").strip()
                    if mid and mid not in models[int(row["PropertyId"])]:
                        models[int(row["PropertyId"])].append(mid)
            self._models = dict(models)
        return self._models

    def _load(self) -> list[KitSpace]:
        if self._spaces is not None:
            return self._spaces
        self._require()
        enums = json.loads((self.kit_dir / self.ENUMS).read_text())
        service_types: dict[str, str] = enums.get("ServiceType", {})
        space_uses: dict[str, str] = enums.get("SpaceUse", {})
        spaces: list[KitSpace] = []
        with (self.kit_dir / self.LISTINGS).open() as f:
            for line in f:
                rec = json.loads(line)
                fl = rec.get("forLeaseSummary")
                if rec.get("listingStatus") != LISTING_ACTIVE or not fl:
                    continue
                if rec.get("listingType") not in LEASE_LISTING_TYPES or rec.get("monetaryUnit") not in (
                    1,
                    None,
                ):
                    continue
                photos = [
                    _photo(a["attachmentUri"])
                    for a in rec.get("attachments") or []
                    if a.get("attachmentUri")
                    and a.get("videoExternalReferenceType") not in MATTERPORT_REF_TYPES
                ][:8]
                for s in fl.get("spaces") or []:
                    sk = self._space(rec, s, photos, service_types, space_uses)
                    if sk is not None:
                        spaces.append(sk)
        self._estimate_missing_opex(spaces)
        self._spaces = spaces
        return spaces

    def _estimate_missing_opex(self, spaces: list[KitSpace]) -> None:
        """Net leases without quoted CAM would look artificially cheap next to ones that quote it. Fill them
        with the median CAM ($/SF/yr) quoted by net-lease spaces in the same state in this kit, flagged as an
        estimate (needs >= 10 quotes)."""
        quotes: dict[str, list[float]] = defaultdict(list)
        for sp in spaces:
            if sp.listing["lease_service_type"] in NET_SERVICE_TYPES and sp.cam_per_sf_year:
                quotes[sp.address["state"]].append(sp.cam_per_sf_year)
        for state, vals in quotes.items():
            if len(vals) < 10:
                continue
            vals.sort()
            median = (
                vals[len(vals) // 2]
                if len(vals) % 2
                else (vals[len(vals) // 2 - 1] + vals[len(vals) // 2]) / 2
            )
            for sp in spaces:
                lst = sp.listing
                if (
                    sp.address["state"] == state
                    and lst["lease_service_type"] in NET_SERVICE_TYPES
                    and lst["nnn_charges_monthly"] is None
                ):
                    lst["nnn_charges_monthly"] = _cents(Decimal(str(median)) * Decimal(str(sp.sqft)) / 12)
                    lst["nnn_is_estimate"] = True
                    lst["disclaimer"] += (
                        f" Operating costs not quoted; estimated at ${median:g}/SF/yr, the median CAM quoted by "
                        f"{len(vals)} {state} net-lease spaces in the kit."
                    )

    def _space(self, rec, s, photos, service_types, space_uses) -> KitSpace | None:
        if s.get("dateOffMarket") or s.get("isLeasePending") or s.get("isInventoryOffMarket"):
            return None
        use = s.get("spaceUse")
        if use not in USE_PREFERENCE:
            return None
        sqft = s.get("squareFeetMax") or s.get("squareFeetMin")
        if not sqft or sqft <= 0:
            return None
        addr = s.get("marketingAddress") or rec.get("address") or {}
        street = _street(addr)
        state = (addr.get("state") or "").upper()
        if not street or state not in MARKET_BY_STATE:
            return None
        basis = s.get("rateBasis")
        rate = s.get("rateLow") or 0
        months_per_period = {1: 1, 2: 12}.get(basis)
        rent = (
            _cents(Decimal(str(rate)) * Decimal(str(sqft)) / months_per_period)
            if rate > 0 and months_per_period
            else None
        )
        service = service_types.get(str(s.get("serviceType")), "Unknown")
        cam = s.get("commonAreaMaintenanceAmount")
        nnn = (
            _cents(Decimal(str(cam)) * Decimal(str(sqft)) / months_per_period)
            if cam and months_per_period
            else (0 if service in {"FullService", "FullServiceGrossNoPass"} else None)
        )
        term = None
        if s.get("leaseTermLow"):
            term = {1: 12, 2: 1}.get(s.get("leaseTermType"))
            term = s["leaseTermLow"] * term if term else None
        notes = " ".join(n.get("value") or "" for n in s.get("notes") or [])
        buildout = next((k for k, pat in BUILDOUT_PATTERNS if pat.search(notes)), None)
        suite = (s.get("suite") or "").strip() or None
        use_name = space_uses.get(str(use), "Unknown")
        rate_note = (
            f"asking ${rate:g}/SF/{'mo' if basis == 1 else 'yr'} ({service})"
            if rent is not None
            else "no asking rate recorded (call for rate)"
        )
        disclaimer = (
            f"CoStar data kit snapshot (certified {(_date(rec.get('certifiedFreshDate')) or 'n/a')}); {rate_note}. "
            "Asking terms, not achieved rent."
        )
        if buildout:
            disclaimer += f" Build-out '{buildout}' inferred from listing notes."
        floor_plan = (s.get("floorPlan") or {}).get("attachmentUri")
        listing = {
            "provider": "costar",
            "provider_listing_id": f"{rec['id']}:{s['id']}",
            "listing_type": "LEASE",
            "property_type": use_name,
            "available_area_sqm": round(sqft * SQFT_TO_SQM, 1),
            "asking_rent_monthly": rent,
            "nnn_charges_monthly": nnn,
            "nnn_is_estimate": False,
            "lease_term_months": term,
            "available_from": _date(s.get("availableOnDate")),
            "existing_buildout": buildout,
            "lease_service_type": service,
            "building_name": rec.get("buildingName"),
            "brokerage_company": rec.get("brokerageCompanyName"),
            "photo_urls": photos,
            "source_record_date": _date(rec.get("certifiedFreshDate")),
            "disclaimer": disclaimer,
        }
        address = {
            "street_address": street,
            "unit_or_suite": suite,
            "city": (addr.get("city") or "").strip() or MARKET_BY_STATE[state][0].title(),
            "state": state,
            "postal_code": (addr.get("postcode") or "")[:5] or None,
            "parcel_id": None,
            "latitude": addr.get("latitude"),
            "longitude": addr.get("longitude"),
        }
        return KitSpace(
            listing_id=rec["id"],
            space_id=s["id"],
            property_id=s.get("propertyId") or rec.get("propertyId"),
            address=address,
            listing=listing,
            use_rank=USE_PREFERENCE[use],
            area_sqm=listing["available_area_sqm"],
            rent_monthly=rent,
            photo_urls=photos,
            floor_plan_url=_photo(floor_plan) if floor_plan else None,
            sqft=float(sqft),
            cam_per_sf_year=float(cam) * (12 if basis == 1 else 1) if cam and months_per_period else None,
        )

    # -- search -------------------------------------------------------------------------------
    def search(self, request: PropertySearchRequest) -> list[RawListing]:
        state = request.state.upper()
        market = MARKET_BY_STATE.get(state)
        if market is None:
            return []
        whole_market = request.city.strip().upper() == market[0]
        target = request.min_area_sqm
        cands = []
        self.skipped_no_rate = 0
        for sp in self._load():
            if sp.address["state"] != state:
                continue
            if not whole_market and sp.address["city"].upper() != request.city.strip().upper():
                continue
            if request.neighborhoods and not any(
                n.upper() in sp.address["city"].upper() for n in request.neighborhoods
            ):
                continue
            if target is not None and (sp.area_sqm < target or sp.area_sqm > target * 2.5):
                continue
            if sp.rent_monthly is None:
                self.skipped_no_rate += 1  # cannot rank total cost without an asking rent
                continue
            if request.max_monthly_rent is not None and sp.rent_monthly > request.max_monthly_rent:
                continue
            cands.append(sp)
        cands.sort(key=lambda sp: (sp.use_rank, sp.rent_monthly, abs(sp.area_sqm - (target or sp.area_sqm))))
        out: list[RawListing] = []
        per_property: dict[int, int] = defaultdict(int)
        models = self.matterport_models()
        for sp in cands:
            if per_property[sp.property_id] >= 2:  # keep the shortlist diverse across buildings
                continue
            per_property[sp.property_id] += 1
            interior = [
                {
                    "provider": "matterport",
                    "asset_id": mid,
                    "asset_type": "matterport_model",
                    "has_measured_geometry": False,  # set True only after the Model API returns floor geometry
                    "url": f"https://my.matterport.com/show/?m={mid}",
                }
                for mid in models.get(sp.property_id, [])
            ]
            if sp.floor_plan_url:
                interior.append(
                    {
                        "provider": "costar",
                        "asset_id": f"floorplan-{sp.space_id}",
                        "asset_type": "floor_plan_image",
                        "url": sp.floor_plan_url,
                    }
                )
            out.append(
                RawListing(
                    "costar",
                    sp.listing["provider_listing_id"],
                    DataOrigin.FIXTURE_REAL,
                    sp.address,
                    {**sp.listing, "costar_property_id": sp.property_id},
                    interior,
                    [],
                )
            )
            if len(out) >= request.limit:
                break
        return out

    def status(self):
        ok = self.available()
        return status_entry(
            "costar",
            self.mode,
            IntegrationStatus.LIVE if ok else IntegrationStatus.BLOCKED,
            ok,
            "CoStar hackathon data kit (snapshot of genuine records) read from COSTAR_KIT_DIR."
            if ok
            else "CoStar data kit not extracted; set COSTAR_KIT_DIR.",
        )
