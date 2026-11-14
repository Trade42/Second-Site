"use client";

import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import type { ProjectEnvelope } from "@second-site/contracts";
import { apiClient, problemMessage, type ApiClient } from "@/lib/api";

type State = {
  token: string | null;
  projectId: string | null;
  envelope: ProjectEnvelope | null;
  client: ApiClient;
  error: string | null;
  busy: boolean;
  seedDemo: () => Promise<void>;
  refresh: () => Promise<void>;
};

const Ctx = createContext<State | null>(null);
const KEY = "second-site.session";

function load(): { token: string | null; projectId: string | null } {
  try {
    return JSON.parse(localStorage.getItem(KEY) ?? "{}");
  } catch {
    return { token: null, projectId: null };
  }
}

export function ProjectProvider({ children }: { children: ReactNode }) {
  const [token, setToken] = useState<string | null>(null);
  const [projectId, setProjectId] = useState<string | null>(null);
  const [envelope, setEnvelope] = useState<ProjectEnvelope | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const client = useMemo(() => apiClient(token), [token]);

  useEffect(() => {
    // Read the persisted session after mount (localStorage is unavailable during SSR).
    void Promise.resolve(load()).then((s) => {
      setToken(s.token ?? null);
      setProjectId(s.projectId ?? null);
    });
  }, []);

  const save = (t: string | null, p: string | null) => {
    try {
      localStorage.setItem(KEY, JSON.stringify({ token: t, projectId: p }));
    } catch {
      /* storage unavailable: session-only */
    }
  };

  const fetchEnvelope = useCallback(async () => {
    if (!token || !projectId) return null;
    return client.GET("/api/v1/projects/{project_id}", { params: { path: { project_id: projectId } } });
  }, [client, token, projectId]);

  const apply = useCallback((res: Awaited<ReturnType<typeof fetchEnvelope>>) => {
    if (!res) return;
    if (res.error) {
      setError(problemMessage(res.error));
      setEnvelope(null);
    } else {
      setError(null);
      setEnvelope(res.data ?? null);
    }
  }, []);

  const refresh = useCallback(async () => apply(await fetchEnvelope()), [fetchEnvelope, apply]);

  useEffect(() => {
    let cancelled = false;
    void fetchEnvelope().then((res) => {
      if (!cancelled) apply(res);
    });
    return () => {
      cancelled = true;
    };
  }, [fetchEnvelope, apply]);

  const seedDemo = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const anon = apiClient(null);
      const tok = await anon.POST("/api/v1/dev/token");
      if (!tok.data) throw new Error(problemMessage(tok.error));
      const authed = apiClient(tok.data.access_token);
      const fx = await authed.GET("/api/v1/dev/fixtures/project");
      const body = fx.data as { project_name: string; market: { city: string; state: string }; business_profile: Record<string, unknown> };
      const created = await authed.POST("/api/v1/projects", {
        body: { project_name: body.project_name, market: body.market, business_profile: body.business_profile },
      });
      if (!created.data) throw new Error(problemMessage(created.error));
      const pid = created.data.project.project_id;
      await authed.POST("/api/v1/projects/{project_id}/properties/search", {
        params: { path: { project_id: pid } },
        body: { city: body.market.city, state: body.market.state },
      });
      setToken(tok.data.access_token);
      setProjectId(pid);
      save(tok.data.access_token, pid);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not reach the API");
    } finally {
      setBusy(false);
    }
  }, []);

  return (
    <Ctx.Provider value={{ token, projectId, envelope, client, error, busy, seedDemo, refresh }}>{children}</Ctx.Provider>
  );
}

export function useProject(): State {
  const v = useContext(Ctx);
  if (!v) throw new Error("useProject must be used inside ProjectProvider");
  return v;
}
