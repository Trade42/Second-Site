import type { CostScenario, PropertyDigitalProfile, RankedProperty } from "@second-site/contracts";
import Link from "next/link";
import { Badge, Card } from "@/components/ui/primitives";
import { sqft, usd } from "@/lib/format";
import { EvidenceStatus } from "@/components/EvidenceStatus";

export function PropertyCard({ p, rank, scenario }: { p: PropertyDigitalProfile; rank?: RankedProperty; scenario?: CostScenario }) {
  const l = p.commercial_listing;
  return (
    <Card selected={rank?.rank === 1}>
      <div className="flex items-start justify-between">
        <div>
          <div className="text-xs uppercase tracking-wide text-ink-muted">{rank ? `Rank #${rank.rank}` : "Candidate"}</div>
          <Link href={`/intelligence?property=${p.property_id}`} className="text-base font-semibold hover:underline">
            {p.property_identity.normalized_address}
          </Link>
        </div>
        {l && <Badge tone={l.data_origin === "LIVE" ? "good" : "warn"}>{l.data_origin === "LIVE" ? l.provider : "Synthetic listing"}</Badge>}
      </div>
      <dl className="mt-4 grid grid-cols-3 gap-3 text-sm">
        <div><dt className="text-ink-muted">Asking rent</dt><dd>{usd(l?.asking_rent_monthly)}/mo</dd></div>
        <div><dt className="text-ink-muted">Area</dt><dd>{sqft(l?.available_area_sqm)}</dd></div>
        <div><dt className="text-ink-muted">{scenario?.comparison_horizon_months ?? 36}-mo total</dt><dd className="font-medium">{usd(scenario?.long_term_cost)}</dd></div>
      </dl>
      <div className="mt-4 flex flex-wrap gap-2">
        <EvidenceStatus assessments={p.infrastructure_assessments ?? []} />
        {rank?.disqualified && <Badge tone="bad">Fails a hard constraint</Badge>}
      </div>
    </Card>
  );
}
