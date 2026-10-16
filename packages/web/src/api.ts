// Types and transport for the Pit HTTP API. Shapes mirror crates/pit/src/model.rs.

export type Status = "queued" | "running" | "passed" | "failed" | "cached" | "skipped" | "error";

export interface TaskView {
  id: string;
  name: string;
  deps: string[];
  status: Status;
  actionKey: string | null;
  exitCode: number | null;
  durationMs: number | null;
  logDigest: string | null;
  message: string | null;
  startedAt: number | null;
  finishedAt: number | null;
}

export interface GroupView {
  id: string;
  name: string;
  os: string;
  status: Status;
  worker: string | null;
  prepareMs: number | null;
  message: string | null;
  startedAt: number | null;
  finishedAt: number | null;
  tasks: TaskView[];
}

export interface RunView {
  id: string;
  repo: string;
  sha: string;
  ref: string | null;
  trigger: string;
  status: Status;
  overlayFiles: number;
  message: string | null;
  createdAt: number;
  startedAt: number | null;
  finishedAt: number | null;
  groups: GroupView[];
}

export interface PitEvent {
  kind: string;
  runId?: string;
  groupId?: string;
  taskId?: string;
  data: any;
}

export interface Worker {
  name: string;
  os: string;
  arch: string;
  cpus: number;
  backends: string[];
  kvm: boolean;
  lastSeen: number;
  online: boolean;
  currentRun: string | null;
}

export type QueueStatus = "queued" | "testing" | "passed" | "landed" | "failed" | "removing" | "removed";

export interface QueueEntry {
  id: string;
  repo: string;
  source: string;
  headSha: string;
  target: string;
  status: QueueStatus;
  lane: string | null;
  position: number | null;
  affected: string[];
  candidateSha: string | null;
  runId: string | null;
  landedSha: string | null;
  reason: string | null;
  createdAt: number;
  updatedAt: number;
}

export interface Stats {
  runs: number;
  tasks: number;
  tasksCached: number;
  tasksRun: number;
  cacheEntries: number;
  cacheHits: number;
  timeSavedMs: number;
  remoteCacheArtifacts: number;
  casBlobs: number;
  casBytes: number;
}

export interface Health {
  ok: boolean;
  version: string;
  auth: boolean;
  github: boolean;
}

// --- auth -------------------------------------------------------------------

const TOKEN_KEY = "pit.token";

function initToken(): string | null {
  try {
    const url = new URL(window.location.href);
    const t = url.searchParams.get("token");
    if (t) {
      localStorage.setItem(TOKEN_KEY, t);
      url.searchParams.delete("token");
      history.replaceState(history.state, "", url.pathname + url.search + url.hash);
      return t;
    }
    return localStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

const token = initToken();

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

async function request(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers);
  if (token) headers.set("Authorization", `Bearer ${token}`);
  const res = await fetch(path, { ...init, headers });
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`;
    try {
      const body = await res.json();
      if (body?.error) msg = body.error;
    } catch {
      /* not json */
    }
    throw new ApiError(res.status, msg);
  }
  return res;
}

async function getJson<T>(path: string): Promise<T> {
  return (await request(path)).json() as Promise<T>;
}

export const api = {
  runs: (limit = 50) => getJson<RunView[]>(`/api/runs?limit=${limit}`),
  run: (id: string) => getJson<RunView>(`/api/runs/${encodeURIComponent(id)}`),
  cancel: (id: string) => request(`/api/runs/${encodeURIComponent(id)}/cancel`, { method: "POST" }),
  taskLog: async (id: string) => (await request(`/api/tasks/${encodeURIComponent(id)}/log`)).text(),
  workers: () => getJson<Worker[]>("/api/workers"),
  queue: () => getJson<QueueEntry[]>("/api/queue"),
  stats: () => getJson<Stats>("/api/stats"),
  health: () => getJson<Health>("/api/health"),
};

export function eventSource(path: string): EventSource {
  const url = token ? `${path}${path.includes("?") ? "&" : "?"}token=${encodeURIComponent(token)}` : path;
  return new EventSource(url);
}

// --- shared global event stream ---------------------------------------------

type Listener = (e: PitEvent) => void;
const listeners = new Set<Listener>();
let globalSource: EventSource | null = null;

let retry: ReturnType<typeof setTimeout> | undefined;

function connectGlobal() {
  let dropped = false;
  const es = eventSource("/api/events");
  globalSource = es;
  es.onopen = () => {
    // After a reconnect, events may have been missed: ask everyone to refetch.
    if (dropped) listeners.forEach((l) => l({ kind: "resync", data: null }));
    dropped = false;
  };
  es.onerror = () => {
    dropped = true;
    // The browser retries on network errors, but gives up on HTTP errors.
    if (es.readyState === EventSource.CLOSED && globalSource === es) {
      clearTimeout(retry);
      retry = setTimeout(() => {
        if (globalSource === es && listeners.size > 0) connectGlobal();
      }, 3000);
    }
  };
  es.onmessage = (m) => {
    let e: PitEvent;
    try {
      e = JSON.parse(m.data);
    } catch {
      return;
    }
    listeners.forEach((l) => l(e));
  };
}

/**
 * Subscribe to /api/events. One EventSource is shared by every subscriber.
 * A synthetic `resync` event is delivered after the stream reconnects.
 */
export function subscribeAll(fn: Listener): () => void {
  listeners.add(fn);
  if (!globalSource) connectGlobal();
  return () => {
    listeners.delete(fn);
    if (listeners.size === 0 && globalSource) {
      clearTimeout(retry);
      globalSource.close();
      globalSource = null;
    }
  };
}
