"""LA Department of Building and Safety permits via the City's Socrata open-data API.

Verified 2026-11-14 against https://data.lacity.org:
- ysqd-apz7  "Electrical Permits Issued from 2020 to Present (N)"
- 67is-svtd  "Mechanical Permits Issued from 2020 to Present (N)" (plumbing, HVAC, fire sprinkler, pressure vessel)
Columns used: permit_nbr, primary_address, zip_code, apn, pin_nbr, permit_group, permit_type,
permit_sub_type, issue_date, status_desc, status_date, work_desc, lat, lon.
Filtering: SoQL `$where`, pagination: `$limit` / `$offset`. Coverage: issue_date >= 2020-01-01 only.
Unit/suite is NOT a separate column; it is appended to primary_address in inconsistent forms
("302", "STE 302", "Suite 302", "#314", "5th Floor").
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Protocol

import httpx

from app.contracts import DataOrigin, IntegrationStatus, ProviderStatusEntry
from app.integrations.base import FIXTURES_DIR, RetryPolicy, status_entry, with_retries

PROVIDER = "la_open_data"


@dataclass(frozen=True)
class RawPermit:
    dataset: str
    record: dict[str, Any]
    data_origin: DataOrigin
    source_url: str


class LAPermitProvider(Protocol):
    mode: str

    def search_by_address_prefix(
        self, house_number: str, direction: str | None, street_name: str, zip_code: str | None = None
    ) -> list[RawPermit]: ...

    def get_permit(self, permit_number: str) -> list[RawPermit]: ...

    def status(self) -> ProviderStatusEntry: ...


def _soql_escape(value: str) -> str:
    return value.replace("'", "''")


def address_prefix(house_number: str, direction: str | None, street_name: str) -> str:
    parts = [house_number] + ([direction] if direction else []) + [street_name]
    return " ".join(p.upper() for p in parts)


class LiveLAPermitProvider:
    mode = "live"

    def __init__(
        self,
        base_url: str,
        electrical_dataset: str,
        mechanical_dataset: str,
        app_token: str | None = None,
        policy: RetryPolicy | None = None,
        client: httpx.Client | None = None,
    ):
        self.base_url = base_url.rstrip("/")
        self.datasets = [electrical_dataset, mechanical_dataset]
        self.policy = policy or RetryPolicy()
        headers = {"X-App-Token": app_token} if app_token else {}
        self.client = client or httpx.Client(timeout=self.policy.timeout_s, headers=headers)

    def _query(self, dataset: str, params: dict[str, str]) -> list[RawPermit]:
        url = f"{self.base_url}/resource/{dataset}.json"
        out: list[RawPermit] = []
        offset, page = 0, 1000
        while True:
            q = {**params, "$limit": str(page), "$offset": str(offset), "$order": "permit_nbr"}

            def call(q: dict[str, str] = q) -> list[dict[str, Any]]:
                resp = self.client.get(url, params=q)
                resp.raise_for_status()
                return resp.json()

            rows = with_retries(PROVIDER, self.policy, call)
            out += [
                RawPermit(dataset, r, DataOrigin.LIVE, f"{url}?permit_nbr={r.get('permit_nbr')}")
                for r in rows
            ]
            if len(rows) < page:
                return out
            offset += page

    def search_by_address_prefix(self, house_number, direction, street_name, zip_code=None):
        prefix = _soql_escape(address_prefix(house_number, direction, street_name))
        where = f"upper(primary_address) like '{prefix}%'"
        if zip_code:
            where += f" AND zip_code = '{_soql_escape(zip_code)}'"
        return [p for ds in self.datasets for p in self._query(ds, {"$where": where})]

    def get_permit(self, permit_number):
        return [p for ds in self.datasets for p in self._query(ds, {"permit_nbr": permit_number})]

    def status(self):
        return status_entry(
            PROVIDER,
            self.mode,
            IntegrationStatus.LIVE,
            True,
            "Public Socrata API; app token optional. Coverage 2020-present.",
        )


class FixtureLAPermitProvider:
    """Serves the committed real snapshot (fixtures/permits/la_permits_snapshot.json)."""

    mode = "fixture"

    def __init__(self, path=None):
        self.path = path or FIXTURES_DIR / "permits" / "la_permits_snapshot.json"
        doc = json.loads(self.path.read_text())
        self._rows = [(ds, r) for ds, rows in doc["records"].items() for r in rows]

    def _wrap(self, ds: str, r: dict[str, Any]) -> RawPermit:
        return RawPermit(
            ds,
            r,
            DataOrigin.FIXTURE_REAL,
            f"https://data.lacity.org/resource/{ds}.json?permit_nbr={r['permit_nbr']}",
        )

    def search_by_address_prefix(self, house_number, direction, street_name, zip_code=None):
        prefix = address_prefix(house_number, direction, street_name)
        return [
            self._wrap(ds, r)
            for ds, r in self._rows
            if r["primary_address"].upper().startswith(prefix)
            and (zip_code is None or r.get("zip_code") == zip_code)
        ]

    def get_permit(self, permit_number):
        return [self._wrap(ds, r) for ds, r in self._rows if r["permit_nbr"] == permit_number]

    def status(self):
        return status_entry(
            PROVIDER,
            self.mode,
            IntegrationStatus.MOCK,
            False,
            "Real public records snapshot (FIXTURE_REAL); set PERMIT_PROVIDER_MODE=live for live API.",
        )
