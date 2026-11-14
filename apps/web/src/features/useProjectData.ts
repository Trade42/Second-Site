"use client";

import { useProject } from "@/hooks/useProject";
import { useApi } from "@/hooks/useApi";

/** Shared loaders so pages consume the same contract-typed endpoints. */
export function useProperties() {
  const { client, projectId } = useProject();
  return useApi(async () => {
    if (!projectId) return { data: [] };
    return client.GET("/api/v1/projects/{project_id}/properties", { params: { path: { project_id: projectId } } });
  }, `${projectId}:${Boolean(client)}`);
}

export function useRankings() {
  const { client, projectId } = useProject();
  return useApi(async () => {
    if (!projectId) return { data: [] };
    return client.GET("/api/v1/projects/{project_id}/rankings", { params: { path: { project_id: projectId } } });
  }, `${projectId}:${Boolean(client)}`);
}

export function useScenarios() {
  const { client, projectId } = useProject();
  return useApi(async () => {
    if (!projectId) return { data: [] };
    return client.GET("/api/v1/projects/{project_id}/cost-scenarios", { params: { path: { project_id: projectId } } });
  }, `${projectId}:${Boolean(client)}`);
}

export function useEvents() {
  const { client, projectId } = useProject();
  return useApi(async () => {
    if (!projectId) return { data: { events: [], next_cursor: 0 } };
    return client.GET("/api/v1/projects/{project_id}/events", { params: { path: { project_id: projectId }, query: { after: 0, limit: 500 } } });
  }, `${projectId}:${Boolean(client)}`);
}

export function useLayouts() {
  const { client, projectId } = useProject();
  return useApi(async () => {
    if (!projectId) return { data: [] };
    return client.GET("/api/v1/projects/{project_id}/layouts", { params: { path: { project_id: projectId } } });
  }, `${projectId}:${Boolean(client)}`);
}
