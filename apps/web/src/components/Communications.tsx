"use client";

import type { CallSession, CommunicationSession, OwnerHandoffRequest } from "@second-site/contracts";
import { useState } from "react";
import { Badge, Button, Card, cn } from "@/components/ui/primitives";
import { useApi } from "@/hooks/useApi";
import { useProject } from "@/hooks/useProject";
import { problemMessage } from "@/lib/api";

const CALL_TONE: Record<string, "good" | "warn" | "bad" | "neutral" | "gold"> = {
  COMPLETED: "good",
  HUMAN_JOINED: "gold",
  HANDOFF_PENDING: "warn",
  OWNER_JOINING: "warn",
  FAILED: "bad",
  DECLINED: "bad",
};

/** Owner's Photon channel: register the handle you text from; linking completes when you text the agent first. */
export function OwnerChannelCard() {
  const { client, token } = useProject();
  const view = useApi(async () => client.GET("/api/v1/owner/channel"), `${token}`);
  const [handle, setHandle] = useState("");
  const [msg, setMsg] = useState<string | null>(null);
  const save = async () => {
    const { error } = await client.PUT("/api/v1/owner/channel", { body: { handle, platform: "imessage" } });
    setMsg(error ? problemMessage(error) : null);
    await view.reload();
  };
  const poll = async () => {
    const { data, error } = await client.POST("/api/v1/owner/channel/poll");
    setMsg(error ? problemMessage(error) : `Checked Photon: ${data?.new ?? 0} new message(s).`);
    await view.reload();
  };
  const [sim, setSim] = useState("");
  const simulateText = async () => {
    const { data, error } = await client.POST("/api/v1/owner/channel/simulate-text", { body: { text: sim } });
    setMsg(error ? problemMessage(error) : data?.reply ? `Assistant replied: ${data.reply}` : "No reply (not routed).");
    setSim("");
    await view.reload();
  };
  const ch = view.data?.channel;
  return (
    <Card>
      <div className="flex items-center justify-between">
        <h3 className="font-medium">Text me (Photon)</h3>
        <Badge tone={ch?.status === "LINKED" ? "good" : "warn"}>{ch ? (ch.status === "LINKED" ? "Connected" : "Waiting for your first text") : "Not set up"}</Badge>
      </div>
      <p className="mt-2 text-sm text-ink-muted">{view.data?.instructions ?? "Loading…"}</p>
      <div className="mt-3 flex gap-2">
        <input
          className="min-w-0 flex-1 rounded-md border border-line bg-white px-3 py-1.5 text-sm"
          placeholder={ch?.handle ?? "+1 555 555 0142 or you@icloud.com"}
          value={handle}
          onChange={(e) => setHandle(e.target.value)}
        />
        <Button onClick={save} disabled={!handle}>Save</Button>
        <Button variant="ghost" onClick={poll}>Check texts</Button>
      </div>
      {ch && (
        <div className="mt-3 flex gap-2">
          <input
            className="min-w-0 flex-1 rounded-md border border-dashed border-line bg-white px-3 py-1.5 text-sm"
            placeholder="Demo without Photon keys: simulate a text from you (hi, STATUS, JOIN…)"
            value={sim}
            onChange={(e) => setSim(e.target.value)}
          />
          <Button variant="ghost" onClick={simulateText} disabled={!sim}>Send as me</Button>
        </div>
      )}
      {msg && <p className="mt-2 text-xs text-ink-muted">{msg}</p>}
      <p className="mt-2 text-xs text-ink-muted">Reply JOIN or LATER when a call needs you, STATUS for a summary, STOP to pause. Approvals always happen here, never by text.</p>
    </Card>
  );
}

export function HandoffCard({ handoff, onDone }: { handoff: OwnerHandoffRequest; onDone: () => void }) {
  const { client, projectId } = useProject();
  const [err, setErr] = useState<string | null>(null);
  const respond = async (response: "ACCEPTED" | "DEFERRED") => {
    if (!projectId) return;
    const { error } = await client.POST("/api/v1/projects/{project_id}/handoffs/{handoff_id}/response", {
      params: { path: { project_id: projectId, handoff_id: handoff.handoff_request_id } },
      body: { response },
    });
    setErr(error ? problemMessage(error) : null);
    onDone();
  };
  return (
    <Card selected>
      <div className="flex items-center justify-between">
        <div className="font-medium">A call needs you</div>
        <Badge tone="warn">{handoff.reason.replaceAll("_", " ").toLowerCase()}</Badge>
      </div>
      <p className="mt-2 text-sm">&ldquo;{handoff.question}&rdquo;</p>
      <p className="mt-1 text-xs text-ink-muted">Expires {new Date(handoff.expires_at).toLocaleTimeString()}. If you don&rsquo;t answer, the assistant says you&rsquo;ll follow up and agrees to nothing.</p>
      <div className="mt-3 flex gap-2">
        <Button onClick={() => respond("ACCEPTED")}>Join the call</Button>
        <Button variant="ghost" onClick={() => respond("DEFERRED")}>Later</Button>
      </div>
      {err && <div className="mt-2 text-sm text-danger">{err}</div>}
    </Card>
  );
}

export function CallCard({ call, onChanged }: { call: CallSession; onChanged: () => void }) {
  const { client, projectId } = useProject();
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const simulate = async () => {
    if (!projectId) return;
    setBusy(true);
    const { error } = await client.POST("/api/v1/projects/{project_id}/calls/{call_id}/simulate", {
      params: { path: { project_id: projectId, call_id: call.call_id } },
      body: { owner_action: "accept", answers: {} },
    });
    setErr(error ? problemMessage(error) : null);
    setBusy(false);
    onChanged();
  };
  return (
    <Card>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="font-medium">Call with {call.recipient.name ?? "broker"}</div>
        <div className="flex gap-2">
          {call.simulated && <Badge tone="warn">Simulated (no carrier)</Badge>}
          <Badge tone={CALL_TONE[call.state] ?? "neutral"}>{call.state.replaceAll("_", " ")}</Badge>
        </div>
      </div>
      {call.simulated && call.state === "DIALING" && (
        <div className="mt-3">
          <Button onClick={simulate} disabled={busy}>{busy ? "Running…" : "Run simulated broker call"}</Button>
          <p className="mt-1 text-xs text-ink-muted">Plays scripted broker answers through the real call handlers. Results are labeled simulated.</p>
        </div>
      )}
      {!!call.transcript?.length && (
        <div className="mt-3 max-h-80 space-y-2 overflow-y-auto rounded-md bg-canvas p-3 text-sm">
          {call.transcript.map((t, i) => (
            <div key={i} className={cn("flex", t.speaker === "assistant" ? "justify-start" : t.speaker === "system" ? "justify-center" : "justify-end")}>
              {t.speaker === "system" ? (
                <span className="text-xs text-ink-muted">{t.text}</span>
              ) : (
                <span className={cn("max-w-[85%] rounded-lg px-3 py-1.5", t.speaker === "assistant" ? "bg-white" : t.speaker === "owner" ? "bg-gold-soft" : "bg-sage-soft")}>
                  <span className="block text-[10px] uppercase tracking-wide text-ink-muted">{t.speaker === "counterparty" ? call.recipient.name ?? "broker" : t.speaker}</span>
                  {t.text}
                </span>
              )}
            </div>
          ))}
        </div>
      )}
      {!!call.findings?.length && (
        <div className="mt-3">
          <div className="text-xs uppercase tracking-wide text-ink-muted">Findings (broker-reported, not verified)</div>
          <ul className="mt-1 text-sm">
            {call.findings.map((f, i) => (
              <li key={i}>
                <span className="font-medium">{(f.claim_type ?? "answer").replaceAll("_", " ")}:</span>{" "}
                {f.value === null || f.value === undefined ? <span className="text-warn">unclear</span> : typeof f.value === "boolean" ? (f.value ? "yes" : "no") : `${String(f.value)}${f.unit ? ` ${f.unit}` : ""}`}
              </li>
            ))}
          </ul>
        </div>
      )}
      {!!call.deferred_questions?.length && <p className="mt-2 text-sm text-warn">Deferred to you: {call.deferred_questions.join("; ")}</p>}
      {call.outcome_summary && <p className="mt-2 text-xs text-ink-muted">{call.outcome_summary}</p>}
      {err && <div className="mt-2 text-sm text-danger">{err}</div>}
    </Card>
  );
}

/** Approved outreach with no call yet: the owner starts it explicitly. */
export function StartCallButton({ comm, onStarted }: { comm: CommunicationSession; onStarted: () => void }) {
  const { client, projectId } = useProject();
  const [err, setErr] = useState<string | null>(null);
  const start = async () => {
    if (!projectId) return;
    const { error } = await client.POST("/api/v1/projects/{project_id}/calls", {
      params: { path: { project_id: projectId } },
      body: { communication_id: comm.communication_id },
    });
    setErr(error ? problemMessage(error) : null);
    onStarted();
  };
  return (
    <Card>
      <div className="font-medium">Ready to call {comm.external_participants?.[0]?.name ?? "the broker"}</div>
      <p className="mt-1 text-sm text-ink-muted">{comm.purpose}</p>
      <Button className="mt-3" onClick={start}>Start call</Button>
      {err && <div className="mt-2 text-sm text-danger">{err}</div>}
    </Card>
  );
}
