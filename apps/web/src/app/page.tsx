"use client";

import { ActivityTimeline } from "@/components/ActivityTimeline";
import { CostSummary } from "@/components/CostSummary";
import { Empty, ErrorState, Loading } from "@/components/states";
import { Card } from "@/components/ui/primitives";
import { useEvents, useProperties, useRankings, useScenarios } from "@/features/useProjectData";
import { useProject } from "@/hooks/useProject";

export default function Overview() {
  const { projectId } = useProject();
  const props = useProperties();
  const rankings = useRankings();
  const scenarios = useScenarios();
  const events = useEvents();
  if (!projectId) return <Empty>No project yet. Use “Seed demo project” to run the LA facial-studio slice.</Empty>;
  const latest = rankings.data?.at(-1);
  const byId = Object.fromEntries((props.data ?? []).map((p) => [p.property_id, p]));
  const scen = Object.fromEntries((scenarios.data ?? []).map((s) => [s.property_id, s]));
  return (
    <div className="grid grid-cols-3 gap-6">
      <section className="col-span-2 space-y-4">
        <h2 className="text-lg font-semibold">Recommendation</h2>
        {rankings.loading ? <Loading /> : rankings.error ? <ErrorState message={rankings.error} /> : !latest ? (
          <Empty>No ranking yet.</Empty>
        ) : (
          <>
            <div className="grid grid-cols-2 gap-4">
              {latest.ranked_properties.map((r) =>
                scen[r.property_id] ? (
                  <CostSummary key={r.property_id} s={scen[r.property_id]}
                    title={`#${r.rank} ${byId[r.property_id]?.property_identity.street_address ?? r.property_id}`} />
                ) : null,
              )}
            </div>
            <Card>
              <div className="text-sm font-medium">Why (deterministic ranking v{latest.ranking_version})</div>
              <ul className="mt-2 space-y-2 text-sm text-ink-muted">
                {latest.ranked_properties.map((r) => <li key={r.property_id}>{latest.explanations[r.property_id]}</li>)}
              </ul>
            </Card>
          </>
        )}
      </section>
      <section>
        <h2 className="mb-4 text-lg font-semibold">Agent activity</h2>
        {events.loading ? <Loading /> : events.error ? <ErrorState message={events.error} /> : (
          <ActivityTimeline events={(events.data?.events ?? []).slice(-12).reverse()} />
        )}
      </section>
    </div>
  );
}
