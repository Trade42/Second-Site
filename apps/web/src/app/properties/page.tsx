"use client";

import { PropertyCard } from "@/components/PropertyCard";
import { Empty, ErrorState, Loading } from "@/components/states";
import { useProperties, useRankings, useScenarios } from "@/features/useProjectData";

export default function ExploreProperties() {
  const props = useProperties();
  const rankings = useRankings();
  const scenarios = useScenarios();
  if (props.loading) return <Loading label="Loading properties" />;
  if (props.error) return <ErrorState message={props.error} />;
  if (!props.data?.length) return <Empty>No candidate properties yet.</Empty>;
  const latest = rankings.data?.at(-1);
  const rank = Object.fromEntries((latest?.ranked_properties ?? []).map((r) => [r.property_id, r]));
  const scen = Object.fromEntries((scenarios.data ?? []).map((s) => [s.property_id, s]));
  const ordered = [...props.data].sort((a, b) => (rank[a.property_id]?.rank ?? 99) - (rank[b.property_id]?.rank ?? 99));
  return (
    <div className="space-y-4">
      <p className="text-sm text-ink-muted">Listing terms are synthetic demo data until CoStar access is verified. Permits are real LADBS records.</p>
      <div className="grid grid-cols-2 gap-4">
        {ordered.map((p) => <PropertyCard key={p.property_id} p={p} rank={rank[p.property_id]} scenario={scen[p.property_id]} />)}
      </div>
    </div>
  );
}
