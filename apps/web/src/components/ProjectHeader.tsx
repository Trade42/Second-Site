"use client";

import { Button, Badge } from "@/components/ui/primitives";
import { ErrorState } from "@/components/states";
import { useProject } from "@/hooks/useProject";

export function ProjectHeader() {
  const { envelope, seedDemo, busy, error, projectId } = useProject();
  return (
    <header className="flex items-start justify-between gap-6">
      <div>
        <h1 className="text-2xl font-semibold">{envelope?.project.project_name ?? "No project loaded"}</h1>
        {envelope ? (
          <div className="mt-1 flex items-center gap-2 text-sm text-ink-muted">
            {envelope.project.market.city}, {envelope.project.market.state}
            <Badge>{envelope.project.project_status}</Badge>
            {envelope.business_profile && <Badge tone="good">{envelope.business_profile.business_type}</Badge>}
          </div>
        ) : (
          <p className="mt-1 text-sm text-ink-muted">Seed the facial-studio demo project (fixture data, clearly labeled).</p>
        )}
        {error && <div className="mt-3"><ErrorState message={error} /></div>}
      </div>
      <Button onClick={seedDemo} disabled={busy}>{busy ? "Seeding…" : projectId ? "Re-seed demo project" : "Seed demo project"}</Button>
    </header>
  );
}
