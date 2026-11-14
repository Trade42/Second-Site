import type { TaskRecord } from "@second-site/contracts";
import { Badge } from "@/components/ui/primitives";

export function TaskStatus({ task }: { task: TaskRecord }) {
  const tone = task.status === "COMPLETED" ? "good" : task.status === "FAILED" ? "bad" : "warn";
  return <Badge tone={tone}>{task.task_type}: {task.status}{task.error_code ? ` (${task.error_code})` : ""}</Badge>;
}
