"use client";

import { Empty, ErrorState, Loading } from "@/components/states";
import { Card } from "@/components/ui/primitives";
import { useLayouts } from "@/features/useProjectData";
import { useApi } from "@/hooks/useApi";
import { useProject } from "@/hooks/useProject";
import { usd } from "@/lib/format";

export default function Procurement() {
  const { client, projectId } = useProject();
  const layouts = useLayouts();
  const first = layouts.data?.[0];
  const bom = useApi(async () => {
    if (!projectId || !first) return { data: undefined };
    return client.GET("/api/v1/projects/{project_id}/layouts/{layout_id}/bom", {
      params: { path: { project_id: projectId, layout_id: first.design_version_id } },
    });
  }, `${projectId}:${first?.design_version_id}`);
  if (layouts.loading || bom.loading) return <Loading />;
  if (!first) return <Empty>Generate a layout in Design Studio first; the BOM is derived from its rooms.</Empty>;
  if (bom.error) return <ErrorState message={bom.error} />;
  if (!bom.data) return <Empty>No BOM.</Empty>;
  return (
    <Card>
      <div className="flex items-center justify-between">
        <h2 className="font-semibold">Bill of materials — design v{first.version_number}</h2>
        <div className="text-sm">Estimated {usd(bom.data.estimated_total)} <span className="text-ink-muted">(illustrative prices)</span></div>
      </div>
      <table className="mt-4 w-full text-sm">
        <thead className="text-left text-ink-muted"><tr><th>Room</th><th>Item</th><th>Qty</th><th>Unit price</th></tr></thead>
        <tbody>
          {bom.data.items.map((i) => (
            <tr key={i.bom_item_id} className="border-t border-line">
              <td className="py-1.5">{i.room_id ?? "project"}</td><td>{i.description}</td>
              <td>{String(i.quantity)} {i.unit}</td><td>{usd(i.unit_price_estimate)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </Card>
  );
}
