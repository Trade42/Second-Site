"use client";

import type { ApprovalRequest } from "@second-site/contracts";
import { useState } from "react";
import { Badge, Button, Card } from "@/components/ui/primitives";
import { useProject } from "@/hooks/useProject";
import { problemMessage } from "@/lib/api";

/** The owner approves the exact payload shown; its hash is sent back and checked server-side. */
export function ApprovalDialog({ approval, onDecided }: { approval: ApprovalRequest; onDecided: () => void }) {
  const { client } = useProject();
  const [err, setErr] = useState<string | null>(null);
  const decide = async (decision: "APPROVED" | "REJECTED") => {
    const { error } = await client.POST("/api/v1/approvals/{approval_id}/decision", {
      params: { path: { approval_id: approval.approval_id } },
      body: { decision, action_payload_hash: approval.action_payload_hash },
    });
    if (error) setErr(problemMessage(error));
    else onDecided();
  };
  return (
    <Card>
      <div className="flex items-center justify-between">
        <div className="font-medium">{approval.summary}</div>
        <Badge tone={approval.approval_status === "PENDING" ? "warn" : "neutral"}>{approval.approval_status}</Badge>
      </div>
      <pre className="mt-3 overflow-x-auto rounded bg-canvas p-3 text-xs">{JSON.stringify(approval.action_payload, null, 2)}</pre>
      <div className="mt-1 text-xs text-ink-muted">Risk: {approval.risk_category} · expires {new Date(approval.expires_at).toLocaleString()}</div>
      {approval.approval_status === "PENDING" && (
        <div className="mt-3 flex gap-2">
          <Button onClick={() => decide("APPROVED")}>Approve exactly this</Button>
          <Button variant="ghost" onClick={() => decide("REJECTED")}>Decline</Button>
        </div>
      )}
      {err && <div className="mt-2 text-sm text-danger">{err}</div>}
    </Card>
  );
}
