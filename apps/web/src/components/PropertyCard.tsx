"use client";

import type { CostScenario, PropertyDigitalProfile, RankedProperty } from "@second-site/contracts";
import Link from "next/link";
import { Badge, Card } from "@/components/ui/primitives";
import { sqft, usd } from "@/lib/format";
import { EvidenceStatus } from "@/components/EvidenceStatus";

/** Honest provenance label for a listing: live API, sponsor data-kit snapshot, or synthetic. */
export function listingSource(p: PropertyDigitalProfile): { label: string; tone: "good" | "warn" } {
  const l = p.commercial_listing;
  if (!l || l.data_origin === "FIXTURE_SYNTHETIC") return { label: "Synthetic listing", tone: "warn" };
  if (l.provider === "costar" && l.data_origin === "FIXTURE_REAL") return { label: "CoStar data kit", tone: "good" };
  return { label: l.provider, tone: "good" };
}

/** Remote provider image that removes itself if the CDN refuses it (no broken-image icons in the demo). */
export function RemoteImg({ src, alt, className }: { src: string; alt: string; className?: string }) {
  // eslint-disable-next-line @next/next/no-img-element
  return <img src={src} alt={alt} className={className} loading="lazy" onError={(e) => { e.currentTarget.style.display = "none"; }} />;
}

export function suiteScan(p: PropertyDigitalProfile) {
  return p.interior_assets?.find((a) => a.provider === "matterport" && a.matches_listing_unit);
}

export function PropertyCard({ p, rank, scenario }: { p: PropertyDigitalProfile; rank?: RankedProperty; scenario?: CostScenario }) {
  const l = p.commercial_listing;
  const src = listingSource(p);
  const photo = l?.photo_urls?.[0];
  const aerial = p.exterior_assets?.find((a) => a.provider === "vexcel")?.url;
  const scan = suiteScan(p);
  return (
    <Card selected={rank?.rank === 1}>
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-xs uppercase tracking-wide text-ink-muted">{rank ? `Rank #${rank.rank}` : "Candidate"}</div>
          <Link href={`/intelligence?property=${p.property_id}`} className="text-base font-semibold hover:underline">
            {p.property_identity.normalized_address}
          </Link>
          {l?.building_name && <div className="truncate text-xs text-ink-muted">{l.building_name}{l.property_type ? ` · ${l.property_type}` : ""}</div>}
        </div>
        <Badge tone={src.tone}>{src.label}</Badge>
      </div>
      {(photo || aerial) && (
        <div className="mt-3 grid grid-cols-2 gap-2">
          {aerial && <RemoteImg src={aerial} alt="Aerial imagery (Vexcel)" className="h-28 w-full rounded-md object-cover" />}
          {photo && <RemoteImg src={photo} alt="Listing photo (CoStar)" className="h-28 w-full rounded-md object-cover" />}
        </div>
      )}
      <dl className="mt-4 grid grid-cols-3 gap-3 text-sm">
        <div><dt className="text-ink-muted">Asking rent</dt><dd>{usd(l?.asking_rent_monthly)}/mo</dd></div>
        <div><dt className="text-ink-muted">Area</dt><dd>{sqft(l?.available_area_sqm)}</dd></div>
        <div><dt className="text-ink-muted">{scenario?.comparison_horizon_months ?? 36}-mo total</dt><dd className="font-medium">{usd(scenario?.long_term_cost)}</dd></div>
      </dl>
      <div className="mt-4 flex flex-wrap gap-2">
        <EvidenceStatus assessments={p.infrastructure_assessments ?? []} />
        {l?.lease_service_type && <Badge>{l.lease_service_type}</Badge>}
        {scan && <Badge tone="good">Matterport scan of this suite</Badge>}
        {rank?.disqualified && <Badge tone="bad">Fails a hard constraint</Badge>}
      </div>
    </Card>
  );
}
