"use client";

import { useSearchParams } from "next/navigation";
import { Suspense, useState } from "react";
import { AssessmentStatusBadge } from "@/components/EvidenceStatus";
import { Empty, ErrorState, Loading } from "@/components/states";
import { Badge, Card, cn } from "@/components/ui/primitives";
import { useProperties } from "@/features/useProjectData";
import { listingSource, RemoteImg } from "@/components/PropertyCard";
import { usd } from "@/lib/format";
import { useProject } from "@/hooks/useProject";
import { problemMessage } from "@/lib/api";
import { Button } from "@/components/ui/primitives";
import Link from "next/link";

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
      <div className="flex flex-wrap gap-2">
        {props.data.map((x) => (
          <button key={x.property_id} onClick={() => setPicked(x.property_id)}
            className={cn("rounded-lg border px-3 py-1.5 text-sm", x.property_id === p.property_id ? "border-gold bg-gold-soft" : "border-line")}>
            {x.property_identity.street_address}{x.property_identity.unit_or_suite ? ` #${x.property_identity.unit_or_suite}` : ""}
          </button>
        ))}
      </div>
      <InsideOut p={p} />
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

type Profile = NonNullable<ReturnType<typeof useProperties>["data"]>[number];

function AskBroker({ p }: { p: Profile }) {
  const { client, projectId } = useProject();
  const [state, setState] = useState<string | null>(null);
  const contact = p.commercial_listing?.broker_contact;
  const open = (p.infrastructure_assessments ?? []).filter((a) => a.status === "UNKNOWN" || a.status === "MAY_REQUIRE_WORK").length;
  if (!contact?.phone) return null;
  const ask = async () => {
    if (!projectId) return;
    const { error } = await client.POST("/api/v1/projects/{project_id}/outreach", {
      params: { path: { project_id: projectId } },
      body: {
        property_id: p.property_id,
        channel: "VOICE",
        recipient: { role: "broker", name: contact.name ?? "Listing broker", phone: contact.phone },
        purpose: `Verify ${open} open item(s) for ${p.property_identity.normalized_address}`,
      },
    });
    setState(error ? problemMessage(error) : "requested");
  };
  return (
    <div className="mt-4 flex flex-wrap items-center gap-3 rounded-md border border-line p-3">
      <div className="min-w-0 flex-1 text-sm">
        <div className="font-medium">Ask {contact.name} ({contact.company || "listing broker"}) to verify {open} open item(s)</div>
        <div className="text-xs text-ink-muted">Your assistant calls only after you approve, identifies itself as automated, and agrees to nothing. CoStar kit phone numbers are anonymized.</div>
      </div>
      {state === "requested" ? (
        <Link href="/activity" className="text-sm underline">Approval requested: review in Activity</Link>
      ) : (
        <Button onClick={ask}>Ask the broker</Button>
      )}
      {state && state !== "requested" && <span className="text-sm text-danger">{state}</span>}
    </div>
  );
}

function InsideOut({ p }: { p: Profile }) {
  const l = p.commercial_listing;
  const src = listingSource(p);
  const aerial = p.exterior_assets?.find((a) => a.provider === "vexcel");
  const scans = (p.interior_assets ?? []).filter((a) => a.provider === "matterport");
  const suite = scans.find((a) => a.matches_listing_unit);
  const plan = p.interior_assets?.find((a) => a.asset_type === "floor_plan_image");
  return (
    <Card>
      <div className="flex items-center justify-between">
        <h2 className="font-semibold">Inside and out</h2>
        <Badge tone={src.tone}>{src.label}</Badge>
      </div>
      {l && (
        <dl className="mt-3 grid grid-cols-2 gap-x-6 gap-y-1 text-sm md:grid-cols-4">
          <div><dt className="text-ink-muted">Asking rent</dt><dd>{usd(l.asking_rent_monthly)}/mo</dd></div>
          <div><dt className="text-ink-muted">Lease type</dt><dd>{l.lease_service_type ?? "Unknown"}</dd></div>
          <div><dt className="text-ink-muted">Operating costs</dt><dd>{l.nnn_charges_monthly == null ? "Not quoted" : `${l.nnn_is_estimate ? "≈" : ""}${usd(l.nnn_charges_monthly)}/mo${l.nnn_is_estimate ? " (estimate)" : ""}`}</dd></div>
          <div><dt className="text-ink-muted">Brokerage</dt><dd>{l.brokerage_company ?? "—"}</dd></div>
        </dl>
      )}
      {l?.disclaimer && <p className="mt-2 text-xs text-ink-muted">{l.disclaimer}</p>}
      <AskBroker p={p} />
      <div className="mt-4 grid gap-4 md:grid-cols-2">
        <div>
          <h3 className="text-sm font-medium">Interior</h3>
          {suite ? (
            <>
              <iframe title="Matterport tour" src={`${suite.url}&play=1&qs=1`} className="mt-2 h-64 w-full rounded-md border border-line" allow="xr-spatial-tracking; fullscreen" />
              <p className="mt-1 text-xs text-ink-muted">Matterport scan &ldquo;{suite.label}&rdquo;{suite.area_sqm ? ` · ${suite.area_sqm.toFixed(0)} m² measured` : ""} · matched to the listed suite.</p>
            </>
          ) : (
            <p className="mt-2 text-sm text-ink-muted">No Matterport scan of this suite.</p>
          )}
          {scans.filter((a) => !a.matches_listing_unit).length > 0 && (
            <p className="mt-2 text-xs text-ink-muted">
              Other scans in this building (not this suite):{" "}
              {scans.filter((a) => !a.matches_listing_unit).slice(0, 4).map((a, i) => (
                <a key={a.asset_id} href={a.url ?? "#"} target="_blank" rel="noreferrer" className="underline">{i ? ", " : ""}{a.label ?? a.asset_id}</a>
              ))}
            </p>
          )}
          {plan?.url && <RemoteImg src={plan.url} alt="Listing floor plan (CoStar)" className="mt-2 max-h-48 rounded-md border border-line" />}
        </div>
        <div>
          <h3 className="text-sm font-medium">Exterior</h3>
          {aerial?.url ? (
            <>
              {/* eslint-disable-next-line @next/next/no-img-element */}
              <img src={aerial.url} alt="Vexcel orthomosaic" className="mt-2 h-64 w-full rounded-md object-cover" loading="lazy" />
              <p className="mt-1 text-xs text-ink-muted">Vexcel aerial imagery{aerial.captured_at ? `, captured ${aerial.captured_at}` : ""}.</p>
            </>
          ) : (
            <p className="mt-2 text-sm text-ink-muted">No aerial imagery.</p>
          )}
          {!!p.building_observations?.length && (
            <ul className="mt-2 space-y-1 text-sm">
              {p.building_observations.map((o) => (
                <li key={o.observation_id}>
                  <span className="font-medium">{o.observation_type.replaceAll("_", " ")}:</span> {String(o.value ?? "unknown")}
                  {o.unit && o.unit !== "count" ? ` ${o.unit}` : ""} <span className="text-xs text-ink-muted">({o.classification.toLowerCase().replaceAll("_", " ")})</span>
                </li>
              ))}
              <li className="text-xs text-ink-muted">{p.building_observations[0].limitations.join(" ")}</li>
            </ul>
          )}
        </div>
      </div>
      {!!l?.photo_urls?.length && (
        <div className="mt-4 grid grid-cols-4 gap-2">
          {l.photo_urls.slice(0, 4).map((u) => (
            <RemoteImg key={u} src={u} alt="Listing photo (CoStar)" className="h-24 w-full rounded-md object-cover" />
          ))}
        </div>
      )}
    </Card>
  );
}

export default function Page() {
  return <Suspense fallback={<Loading />}><Intelligence /></Suspense>;
}
