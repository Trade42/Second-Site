"use client";

import { ActivityTimeline } from "@/components/ActivityTimeline";
import { ApprovalDialog } from "@/components/ApprovalDialog";
import { CallCard, HandoffCard, OwnerChannelCard, StartCallButton } from "@/components/Communications";
import { Empty, ErrorState, Loading } from "@/components/states";
import { useEvents } from "@/features/useProjectData";
import { useApi } from "@/hooks/useApi";
import { useProject } from "@/hooks/useProject";

export default function ActivityPage() {
  const { client, projectId } = useProject();
  const events = useEvents();
  const key = `${projectId}`;
  const path = { params: { path: { project_id: projectId ?? "" } } };
  const approvals = useApi(async () => (projectId ? client.GET("/api/v1/projects/{project_id}/approvals", path) : { data: [] }), key);
  const calls = useApi(async () => (projectId ? client.GET("/api/v1/projects/{project_id}/calls", path) : { data: [] }), key);
  const comms = useApi(async () => (projectId ? client.GET("/api/v1/projects/{project_id}/communications", path) : { data: [] }), key);
  const handoffs = useApi(async () => (projectId ? client.GET("/api/v1/projects/{project_id}/handoffs", path) : { data: [] }), key);
  const reloadAll = async () => {
    await Promise.all([approvals.reload(), calls.reload(), comms.reload(), handoffs.reload(), events.reload()]);
  };
  if (!projectId) return <Empty>No project loaded.</Empty>;
  const callComms = new Set((calls.data ?? []).map((c) => c.communication_id));
  const approved = new Set((approvals.data ?? []).filter((a) => a.approval_status === "APPROVED").map((a) => a.approval_id));
  const ready = (comms.data ?? []).filter((c) => c.channel === "VOICE" && c.approval_id && approved.has(c.approval_id) && !callComms.has(c.communication_id));
  const waiting = (handoffs.data ?? []).filter((h) => h.status === "NOTIFIED");
  return (
    <div className="grid gap-8 lg:grid-cols-2">
      <section className="space-y-6">
        <OwnerChannelCard />
        {waiting.map((h) => <HandoffCard key={h.handoff_request_id} handoff={h} onDone={reloadAll} />)}
        <div>
          <h2 className="mb-3 text-lg font-semibold">Approvals</h2>
          {approvals.loading ? <Loading /> : approvals.error ? <ErrorState message={approvals.error} /> : !approvals.data?.length ? (
            <Empty>No approvals requested. External actions always require your approval first.</Empty>
          ) : (
            <div className="space-y-3">{approvals.data.map((a) => <ApprovalDialog key={a.approval_id} approval={a} onDecided={reloadAll} />)}</div>
          )}
        </div>
        <div>
          <h2 className="mb-3 text-lg font-semibold">Calls</h2>
          <div className="space-y-3">
            {ready.map((c) => <StartCallButton key={c.communication_id} comm={c} onStarted={reloadAll} />)}
            {calls.loading ? <Loading /> : !calls.data?.length && !ready.length ? (
              <Empty>No calls yet. Use &ldquo;Ask the broker&rdquo; on Property Intelligence to request one.</Empty>
            ) : (
              [...(calls.data ?? [])].reverse().map((c) => <CallCard key={c.call_id} call={c} onChanged={reloadAll} />)
            )}
          </div>
        </div>
      </section>
      <section>
        <h2 className="mb-4 text-lg font-semibold">Activity log</h2>
        {events.loading ? <Loading /> : events.error ? <ErrorState message={events.error} /> : (
          <ActivityTimeline events={[...(events.data?.events ?? [])].reverse()} />
        )}
      </section>
    </div>
  );
}
