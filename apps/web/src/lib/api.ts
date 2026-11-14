// Typed API client generated from the backend OpenAPI contract. Do not hand-write request/response types.
import createClient from "openapi-fetch";
import type { paths } from "@second-site/contracts";

export const API_BASE = process.env.NEXT_PUBLIC_API_BASE_URL ?? "http://localhost:8000";

export function apiClient(token: string | null) {
  return createClient<paths>({
    baseUrl: API_BASE,
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  });
}

export type ApiClient = ReturnType<typeof apiClient>;

export function problemMessage(err: unknown): string {
  if (err && typeof err === "object" && "message" in err) return String((err as { message: unknown }).message);
  return "Request failed";
}
