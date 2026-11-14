"use client";

import dynamic from "next/dynamic";
import { useEffect, useState } from "react";
import type { PlanSite } from "@/components/FloorPlanCanvas";
import { Empty, ErrorState, Loading } from "@/components/states";
import { Badge, Button, Card } from "@/components/ui/primitives";
import { useLayouts, useProperties } from "@/features/useProjectData";
import { useProject } from "@/hooks/useProject";
import { problemMessage } from "@/lib/api";

const FloorPlanCanvas = dynamic(() => import("@/components/FloorPlanCanvas"), { ssr: false });

const PROGRAM = [
  { room_type: "treatment", count: 3, min_area_sqm: 10, min_width_m: 2.8, needs_plumbing: true },
  { room_type: "reception", count: 1, min_area_sqm: 12, min_width_m: 3, needs_plumbing: false },
  { room_type: "storage", count: 1, min_area_sqm: 4, min_width_m: 1.5, needs_plumbing: false },
];

export default function DesignStudio() {
  const { client, projectId } = useProject();
  const layouts = useLayouts();
  const props = useProperties();
  const [err, setErr] = useState<string | null>(null);
  const [sites, setSites] = useState<Record<string, PlanSite>>({});
  const geometryIds = [...new Set((layouts.data ?? []).map((d) => d.geometry_id))].join(",");
  useEffect(() => {
    if (!projectId || !geometryIds) return;
    let alive = true;
    (async () => {
      const out: Record<string, PlanSite> = {};
      for (const gid of geometryIds.split(",")) {
        const { data } = await client.GET("/api/v1/projects/{project_id}/layouts/geometry/{geometry_id}", {
          params: { path: { project_id: projectId, geometry_id: gid } },
        });
        if (data) out[gid] = data;
      }
      if (alive) setSites(out);
    })();
    return () => { alive = false; };
  }, [client, projectId, geometryIds]);
  const withGeometry = props.data?.find((p) => p.interior_assets?.some((a) => a.has_measured_geometry));
  const geoAsset = withGeometry?.interior_assets?.find((a) => a.has_measured_geometry);
  const geoLabel = geoAsset?.provider === "matterport" && geoAsset.data_origin === "LIVE"
    ? `Matterport measured scan "${geoAsset.label ?? geoAsset.asset_id}"`
    : "synthetic fixture geometry, not scale-verified";
  const generate = async () => {
    if (!projectId || !withGeometry) return;
    const { error } = await client.POST("/api/v1/projects/{project_id}/layouts/generate", {
      params: { path: { project_id: projectId } },
      body: { property_id: withGeometry.property_id, room_requirements: PROGRAM, candidate_count: 2 },
    });
    if (error) setErr(problemMessage(error));
    await layouts.reload();
  };
  if (!projectId) return <Empty>No project loaded.</Empty>;
  return (
    <div className="space-y-4">
      <div className="flex items-center gap-3">
        <Button onClick={generate} disabled={!withGeometry}>Generate layout candidates</Button>
        <span className="text-sm text-ink-muted">
          {withGeometry ? `Using ${withGeometry.property_identity.normalized_address} (${geoLabel})` : "No candidate has a measured scan of its listed suite."}
        </span>
      </div>
      {err && <ErrorState message={err} />}
      {layouts.loading ? <Loading /> : layouts.error ? <ErrorState message={layouts.error} /> : !layouts.data?.length ? (
        <Empty>No layouts yet.</Empty>
      ) : (
        <div className="grid grid-cols-2 gap-4">
          {layouts.data.map((d) => (
            <Card key={d.design_version_id}>
              <div className="mb-2 flex items-center justify-between">
                <div className="font-medium">Version {d.version_number}</div>
                <Badge tone={d.validation_report.valid ? "good" : "bad"}>{d.validation_report.valid ? "Geometry valid" : "Invalid"}</Badge>
              </div>
              <FloorPlanCanvas design={d} site={sites[d.geometry_id]} width={520} />
              <p className="mt-2 text-xs text-ink-muted">{d.validation_report.disclaimer}</p>
            </Card>
          ))}
        </div>
      )}
    </div>
  );
}
