"use client";

import { useSearchParams } from "next/navigation";
import { Suspense, useState } from "react";
import { AssessmentStatusBadge } from "@/components/EvidenceStatus";
import { Empty, ErrorState, Loading } from "@/components/states";
import { Badge, Card, cn } from "@/components/ui/primitives";
import { useProperties } from "@/features/useProjectData";

function Intelligence() {
  const params = useSearchParams();
  const props = useProperties();
  const [picked, setPicked] = useState<string | null>(params.get("property"));
  if (props.loading) return <Loading />;
  if (props.error) return <ErrorState message={props.error} />;
  if (!props.data?.length) return <Empty>No properties analyzed yet.</Empty>;
  const p = props.data.find((x) => x.property_id === picked) ?? props.data[0];
  return (
    <div className="space-y-6">
      <div className="flex gap-2">
        {props.data.map((x) => (
          <button key={x.property_id} onClick={() => setPicked(x.property_id)}
            className={cn("rounded-lg border px-3 py-1.5 text-sm", x.property_id === p.property_id ? "border-gold bg-gold-soft" : "border-line")}>
            {x.property_identity.street_address}{x.property_identity.unit_or_suite ? ` #${x.property_identity.unit_or_suite}` : ""}
          </button>
        ))}
      </div>
      <Card>
        <h2 className="font-semibold">Infrastructure feasibility</h2>
        <p className="text-xs text-ink-muted">Historical permits document past work; they do not verify current capacity. Unknowns stay unknown.</p>
        <table className="mt-4 w-full text-sm">
          <thead className="text-left text-ink-muted"><tr><th>Requirement</th><th>Status</th><th>Observed</th><th>To verify</th></tr></thead>
          <tbody>
            {(p.infrastructure_assessments ?? []).map((a) => (
              <tr key={a.assessment_id} className="border-t border-line align-top">
                <td className="py-2 pr-3">{a.requirement_id.split(".").slice(1).join(" › ")}</td>
                <td className="py-2 pr-3"><AssessmentStatusBadge status={a.status} /></td>
                <td className="py-2 pr-3">{String(a.observed_value ?? "—")}</td>
                <td className="py-2 text-ink-muted">{a.verification_questions.join(" ")}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </Card>
      <Card>
        <h2 className="font-semibold">Permit evidence ({p.permit_records?.length ?? 0} matched)</h2>
        <ul className="mt-3 space-y-2 text-sm">
          {(p.permit_records ?? []).slice(0, 25).map((r) => (
            <li key={r.permit_record_id} className="flex flex-wrap items-center gap-2">
              <a href={r.source_url ?? "#"} target="_blank" rel="noreferrer" className="font-mono text-xs underline">{r.permit_number}</a>
              <Badge>{r.permit_category}</Badge>
              <Badge tone={r.scope === "UNIT" ? "good" : r.scope === "BUILDING" ? "neutral" : "warn"}>{r.scope}</Badge>
              <Badge tone={r.match_status === "EXACT" ? "good" : "warn"}>{r.match_status}</Badge>
              <span className="text-ink-muted">{r.permit_status} · {r.work_description.slice(0, 90)}</span>
            </li>
          ))}
        </ul>
      </Card>
      <Card>
        <h2 className="font-semibold">Sources</h2>
        <ul className="mt-2 text-sm">
          {(p.source_manifest ?? []).map((m, i) => <li key={i}>{m.provider} · {m.status} · {m.record_count} records {m.note ? `— ${m.note}` : ""}</li>)}
        </ul>
        {!!p.missing_information?.length && <p className="mt-2 text-sm text-warn">Missing: {p.missing_information.join("; ")}</p>}
      </Card>
    </div>
  );
}

export default function Page() {
  return <Suspense fallback={<Loading />}><Intelligence /></Suspense>;
}
