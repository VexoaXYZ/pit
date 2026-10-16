import { api, type RunView, type Stats, type Worker } from "../api";
import {
  Chip,
  Empty,
  ErrorBox,
  Link,
  Loading,
  StatusIcon,
  fmtAgo,
  fmtDuration,
  isActive,
  navigate,
  runDuration,
  shortSha,
  taskSummary,
  useLive,
  useNow,
} from "../lib";

function StatsStrip() {
  const stats = useLive<Stats>(api.stats, (e) => e.kind === "run.finished" || e.kind === "task.finished");
  const workers = useLive<Worker[]>(api.workers, (e) => e.kind === "group.started" || e.kind === "run.finished");
  const s = stats.data;
  const executed = s ? s.tasksCached + s.tasksRun : 0;
  const hit = s && executed > 0 ? Math.round((s.tasksCached / executed) * 100) : null;
  const online = workers.data?.filter((w) => w.online).length;
  const items = [
    { label: "Runs", value: s ? s.runs.toLocaleString() : "–" },
    { label: "Cache hit rate", value: hit == null ? "–" : `${hit}%`, sub: s && executed > 0 ? `${s.tasksCached} of ${executed} tasks` : undefined },
    { label: "Time saved", value: s ? fmtDuration(s.timeSavedMs) : "–", sub: s ? `${s.cacheHits} cache hits` : undefined },
    { label: "Workers online", value: online == null ? "–" : String(online), sub: workers.data ? `${workers.data.length} registered` : undefined, link: "/workers" },
  ];
  return (
    <div className="stats">
      {items.map((it) => {
        const body = (
          <>
            <div className="stat-label">{it.label}</div>
            <div className="stat-value">{it.value}</div>
            <div className="stat-sub">{it.sub ?? " "}</div>
          </>
        );
        return it.link ? (
          <Link key={it.label} to={it.link} className="stat stat-link">
            {body}
          </Link>
        ) : (
          <div key={it.label} className="stat">
            {body}
          </div>
        );
      })}
    </div>
  );
}

function RunRow({ run, now }: { run: RunView; now: number }) {
  const dur = runDuration(run, now);
  return (
    <a
      href={`/runs/${run.id}`}
      className={`run-row s-row-${run.status}`}
      onClick={(e) => {
        if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
        e.preventDefault();
        navigate(`/runs/${run.id}`);
      }}
    >
      <span className="run-status">
        <StatusIcon status={run.status} size={16} />
      </span>
      <span className="run-main">
        <span className="run-title">
          <span className="repo">{run.repo}</span>
          <span className="at">@</span>
          <span className="mono">{shortSha(run.sha)}</span>
          {run.overlayFiles > 0 && (
            <span className="overlay" title="Uncommitted files sent with this run">
              +{run.overlayFiles} file{run.overlayFiles === 1 ? "" : "s"}
            </span>
          )}
        </span>
        <span className="run-meta">
          {run.ref && <span className="mono ref">{run.ref}</span>}
          <Chip>{run.trigger}</Chip>
          <span className="muted summary">{taskSummary(run)}</span>
        </span>
      </span>
      <span className="run-right">
        <span className={`mono dur${isActive(run.status) ? " live" : ""}`}>{fmtDuration(dur)}</span>
        <span className="muted ago" title={new Date(run.createdAt).toLocaleString()}>
          {fmtAgo(run.createdAt, now)}
        </span>
      </span>
    </a>
  );
}

export function RunsPage() {
  const runs = useLive<RunView[]>(() => api.runs(50), (e) => e.kind !== "task.log");
  const anyActive = !!runs.data?.some((r) => isActive(r.status));
  const now = useNow(anyActive ? 1000 : 15000);

  return (
    <div className="page">
      <StatsStrip />
      <div className="section-head">
        <h1>Runs</h1>
        {anyActive && (
          <span className="live-tag">
            <span className="dot dot-live" />
            live
          </span>
        )}
      </div>
      {runs.error && <ErrorBox error={runs.error} />}
      {!runs.data && !runs.error && <Loading />}
      {runs.data && runs.data.length === 0 && (
        <Empty title="No runs yet">
          Run <code>pit run</code> in a repo with a <code>pit.config.ts</code>. Runs appear here live as workers pick them up.
        </Empty>
      )}
      {runs.data && runs.data.length > 0 && (
        <div className="list">
          {runs.data.map((r) => (
            <RunRow key={r.id} run={r} now={now} />
          ))}
        </div>
      )}
    </div>
  );
}
