import { useCallback, useEffect, useRef, useState, useSyncExternalStore, type MouseEvent, type ReactNode } from "react";
import { ApiError, subscribeAll, type PitEvent, type RunView, type Status } from "./api";

// --- router -----------------------------------------------------------------

const NAV_EVENT = "pit:navigate";

export function navigate(to: string) {
  if (to === window.location.pathname) return;
  history.pushState(null, "", to);
  window.dispatchEvent(new Event(NAV_EVENT));
  window.scrollTo(0, 0);
}

function subscribePath(cb: () => void) {
  window.addEventListener("popstate", cb);
  window.addEventListener(NAV_EVENT, cb);
  return () => {
    window.removeEventListener("popstate", cb);
    window.removeEventListener(NAV_EVENT, cb);
  };
}

export function usePath(): string {
  return useSyncExternalStore(subscribePath, () => window.location.pathname);
}

export function Link({ to, className, children, title }: { to: string; className?: string; children: ReactNode; title?: string }) {
  const onClick = (e: MouseEvent<HTMLAnchorElement>) => {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    e.preventDefault();
    navigate(to);
  };
  return (
    <a href={to} className={className} onClick={onClick} title={title}>
      {children}
    </a>
  );
}

// --- hooks ------------------------------------------------------------------

/** Current time, re-rendered every `ms` while `active`. */
export function useNow(ms: number, active = true): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    setNow(Date.now());
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms, active]);
  return now;
}

export interface Loaded<T> {
  data: T | undefined;
  error: Error | undefined;
  reload: () => void;
}

/**
 * Fetches `load()` and refetches (debounced) whenever an event from
 * /api/events passes `filter`.
 */
export function useLive<T>(load: () => Promise<T>, filter: (e: PitEvent) => boolean = () => true, deps: unknown[] = []): Loaded<T> {
  const [data, setData] = useState<T>();
  const [error, setError] = useState<Error>();
  const loadRef = useRef(load);
  loadRef.current = load;
  const filterRef = useRef(filter);
  filterRef.current = filter;
  const seq = useRef(0);

  const reload = useCallback(() => {
    const n = ++seq.current;
    loadRef.current().then(
      (d) => {
        if (n !== seq.current) return;
        setData(d);
        setError(undefined);
      },
      (e: Error) => {
        if (n !== seq.current) return;
        setError(e);
      },
    );
  }, []);

  useEffect(() => {
    setData(undefined);
    reload();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const unsub = subscribeAll((e) => {
      if (e.kind === "task.log" || (e.kind !== "resync" && !filterRef.current(e))) return;
      clearTimeout(timer);
      timer = setTimeout(reload, 150);
    });
    return () => {
      unsub();
      clearTimeout(timer);
    };
  }, deps);

  return { data, error, reload };
}

// --- formatting -------------------------------------------------------------

export function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

export function fmtDuration(ms: number | null | undefined): string {
  if (ms == null || ms < 0) return "–";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = ms / 1000;
  if (s < 10) return `${s.toFixed(1)}s`;
  if (s < 60) return `${Math.round(s)}s`;
  const m = Math.floor(s / 60);
  const rs = Math.round(s % 60);
  if (m < 60) return rs ? `${m}m ${rs}s` : `${m}m`;
  const h = Math.floor(m / 60);
  const rm = m % 60;
  return rm ? `${h}h ${rm}m` : `${h}h`;
}

export function fmtAgo(ts: number, now: number): string {
  const s = Math.max(0, Math.round((now - ts) / 1000));
  if (s < 5) return "just now";
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  if (d < 30) return `${d}d ago`;
  return new Date(ts).toLocaleDateString();
}

export function fmtTime(ts: number): string {
  return new Date(ts).toLocaleString();
}

export function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

export function isActive(s: Status): boolean {
  return s === "queued" || s === "running";
}

export function runDuration(run: RunView, now: number): number | null {
  if (run.finishedAt) return run.finishedAt - run.createdAt;
  if (isActive(run.status)) return now - run.createdAt;
  return null;
}

export function taskSummary(run: RunView): string {
  const tasks = run.groups.flatMap((g) => g.tasks);
  if (tasks.length === 0) return run.status === "queued" ? "planning" : "no tasks";
  const count = (s: Status) => tasks.filter((t) => t.status === s).length;
  const parts = [`${tasks.length} task${tasks.length === 1 ? "" : "s"}`];
  const failed = count("failed") + count("error");
  const running = count("running");
  const cached = count("cached");
  if (failed) parts.push(`${failed} failed`);
  if (running) parts.push(`${running} running`);
  if (cached) parts.push(`${cached} cached`);
  return parts.join(" · ");
}

// --- small components -------------------------------------------------------

const STATUS_LABEL: Record<Status, string> = {
  queued: "Queued",
  running: "Running",
  passed: "Passed",
  failed: "Failed",
  cached: "Cached",
  skipped: "Skipped",
  error: "Error",
};

export function StatusIcon({ status, size = 14 }: { status: Status | string; size?: number }) {
  const s = status as Status;
  const common = { width: size, height: size, viewBox: "0 0 16 16", className: `sicon s-${s}`, "aria-label": STATUS_LABEL[s] ?? s, role: "img" };
  switch (s) {
    case "passed":
      return (
        <svg {...common}>
          <circle cx="8" cy="8" r="7" fill="currentColor" />
          <path d="M4.8 8.2l2.1 2.1 4.3-4.5" fill="none" stroke="var(--bg)" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      );
    case "cached":
      return (
        <svg {...common}>
          <circle cx="8" cy="8" r="7" fill="currentColor" />
          <path d="M9 3.6L5.4 8.6h2.5L7 12.4l3.6-5H8.1z" fill="var(--bg)" />
        </svg>
      );
    case "failed":
    case "error":
      return (
        <svg {...common}>
          <circle cx="8" cy="8" r="7" fill="currentColor" />
          <path d="M5.6 5.6l4.8 4.8M10.4 5.6l-4.8 4.8" stroke="var(--bg)" strokeWidth="1.8" strokeLinecap="round" />
        </svg>
      );
    case "running":
      return (
        <svg {...common}>
          <circle cx="8" cy="8" r="6.2" fill="none" stroke="currentColor" strokeOpacity="0.3" strokeWidth="1.8" />
          <path d="M8 1.8a6.2 6.2 0 0 1 6.2 6.2" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" className="spin" />
          <circle cx="8" cy="8" r="2.4" fill="currentColor" className="pulse" />
        </svg>
      );
    case "skipped":
      return (
        <svg {...common}>
          <circle cx="8" cy="8" r="6.2" fill="none" stroke="currentColor" strokeWidth="1.6" />
          <path d="M5 8h6" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
        </svg>
      );
    default:
      return (
        <svg {...common}>
          <circle cx="8" cy="8" r="6.2" fill="none" stroke="currentColor" strokeWidth="1.6" strokeDasharray="2.6 2.2" />
        </svg>
      );
  }
}

export function StatusBadge({ status }: { status: Status | string }) {
  return (
    <span className={`badge s-${status}`}>
      <StatusIcon status={status} size={12} />
      {STATUS_LABEL[status as Status] ?? status}
    </span>
  );
}

export function Chip({ children, tone, title }: { children: ReactNode; tone?: string; title?: string }) {
  return (
    <span className={`chip${tone ? ` chip-${tone}` : ""}`} title={title}>
      {children}
    </span>
  );
}

export function ErrorBox({ error }: { error: Error }) {
  const unauthorized = error instanceof ApiError && error.status === 401;
  return (
    <div className="notice notice-error">
      {unauthorized ? (
        <>
          This server requires a token. Open the dashboard with <code>?token=YOUR_TOKEN</code> appended to the URL; it is remembered in this
          browser.
        </>
      ) : (
        <>Could not reach the Pit API: {error.message}</>
      )}
    </div>
  );
}

export function Empty({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="empty">
      <div className="empty-title">{title}</div>
      {children && <div className="empty-body">{children}</div>}
    </div>
  );
}

export function Loading() {
  return <div className="loading">Loading…</div>;
}
