"use client";

import { useEffect, useRef, useState } from "react";
import { problemMessage } from "@/lib/api";

type Result<T> = { data?: T; error?: unknown };

/**
 * Loader with consistent loading/error/data states. `key` must change whenever the request inputs change
 * (e.g. `${projectId}`); `reload()` re-runs it. State is only set asynchronously (React compiler friendly).
 */
export function useApi<T>(fn: () => Promise<Result<T>>, key: string) {
  const fnRef = useRef(fn);
  const [version, setVersion] = useState(0);
  const [state, setState] = useState<{ data: T | null; error: string | null; settled: string | null }>({
    data: null,
    error: null,
    settled: null,
  });
  const requestKey = `${key}#${version}`;

  useEffect(() => {
    fnRef.current = fn;
  });

  useEffect(() => {
    let cancelled = false;
    fnRef
      .current()
      .then((res) => {
        if (cancelled) return;
        if (res.error) setState({ data: null, error: problemMessage(res.error), settled: requestKey });
        else setState({ data: (res.data ?? null) as T | null, error: null, settled: requestKey });
      })
      .catch((e: unknown) => {
        if (!cancelled) setState({ data: null, error: e instanceof Error ? e.message : "Request failed", settled: requestKey });
      });
    return () => {
      cancelled = true;
    };
  }, [requestKey]);

  return {
    data: state.data,
    error: state.error,
    loading: state.settled !== requestKey,
    reload: () => setVersion((v) => v + 1),
  };
}
