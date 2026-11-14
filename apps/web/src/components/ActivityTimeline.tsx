import type { ActivityEvent } from "@second-site/contracts";
import { CheckCircle2, CircleDot, XCircle } from "lucide-react";

export function ActivityTimeline({ events }: { events: ActivityEvent[] }) {
  return (
    <ol className="space-y-3">
      {events.map((e) => {
        const Icon = e.event_type.endsWith("failed") ? XCircle : e.event_type.endsWith("completed") ? CheckCircle2 : CircleDot;
        return (
          <li key={e.event_id} className="flex gap-3 text-sm">
            <Icon className="mt-0.5 h-4 w-4 shrink-0 text-sage" />
            <div>
              <div className="font-medium">{e.event_type}</div>
              <div className="text-xs text-ink-muted">
                {new Date(e.occurred_at).toLocaleTimeString()} · {e.actor_type}:{e.actor_id}
              </div>
            </div>
          </li>
        );
      })}
    </ol>
  );
}
