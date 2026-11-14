"use client";

import { ActivityTimeline } from "@/components/ActivityTimeline";
import { ApprovalDialog } from "@/components/ApprovalDialog";
import { Empty, ErrorState, Loading } from "@/components/states";
import { useEvents } from "@/features/useProjectData";
import { useApi } from "@/hooks/useApi";
import { useProject } from "@/hooks/useProject";

export default function ActivityPage() {
  const { client, projectId } = useProject();
  const events = useEvents();
  const approvals = useApi(async () => {
    if (!projectId) return { data: [] };
    return client.GET("/api/v1/projects/{project_id}/approvals", { params: { path: { project_id: projectId } } });
  }, `${projectId}`);
  if (!projectId) return <Empty>No project loaded.</Empty>;
  return (
    <div className="grid grid-cols-2 gap-8">
      <section>
        <h2 className="mb-4 text-lg font-semibold">Approvals</h2>
        {approvals.loading ? <Loading /> : approvals.error ? <ErrorState message={approvals.error} /> : !approvals.data?.length ? (
          <Empty>No approvals requested. External actions always require your approval first.</Empty>
        ) : (
          <div className="space-y-3">{approvals.data.map((a) => <ApprovalDialog key={a.approval_id} approval={a} onDecided={approvals.reload} />)}</div>
        )}
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
