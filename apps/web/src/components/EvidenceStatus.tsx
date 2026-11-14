import type { InfrastructureAssessment } from "@second-site/contracts";
import { Badge } from "@/components/ui/primitives";

const TONE = { MEETS: "good", MAY_REQUIRE_WORK: "warn", FAILS: "bad", UNKNOWN: "neutral" } as const;

export function EvidenceStatus({ assessments }: { assessments: InfrastructureAssessment[] }) {
  const counts = assessments.reduce<Record<string, number>>((acc, a) => ({ ...acc, [a.status]: (acc[a.status] ?? 0) + 1 }), {});
  return (
    <>
      {(["MEETS", "MAY_REQUIRE_WORK", "FAILS", "UNKNOWN"] as const).map((s) =>
        counts[s] ? <Badge key={s} tone={TONE[s]}>{counts[s]} {s.replaceAll("_", " ").toLowerCase()}</Badge> : null,
      )}
    </>
  );
}

export function AssessmentStatusBadge({ status }: { status: InfrastructureAssessment["status"] }) {
  return <Badge tone={TONE[status]}>{status.replaceAll("_", " ")}</Badge>;
}
