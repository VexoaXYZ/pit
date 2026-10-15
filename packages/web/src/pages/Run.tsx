import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { api, eventSource, type GroupView, type PitEvent, type RunView, type Status, type TaskView } from "../api";
import {
  Chip,
  Empty,
  ErrorBox,
  Link,
  Loading,
  StatusBadge,
  StatusIcon,
  fmtAgo,
  fmtDuration,
  fmtTime,
  isActive,
  runDuration,
  shortSha,
  taskSummary,
  useNow,
} from "../lib";

const done = (s: Status) => !isActive(s);

function patchTask(run: RunView | undefined, taskId: string | undefined, patch: Partial<TaskView>): RunView | undefined {
  if (!run || !taskId) return run;
  return {
    ...run,
    groups: run.groups.map((g) => (g.tasks.some((t) => t.id === taskId) ? { ...g, tasks: g.tasks.map((t) => (t.id === taskId ? { ...t, ...patch } : t)) } : g)),
  };
}

function patchGroup(run: RunView | undefined, groupId: string | undefined, patch: Partial<GroupView>): RunView | undefined {
  if (!run || !groupId) return run;
  return { ...run, groups: run.groups.map((g) => (g.id === groupId ? { ...g, ...patch } : g)) };
}

// eslint-disable-next-line no-control-regex
const ANSI = /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07/g;

function LogPanel({ text, status }: { text: string | undefined; status: Status }) {
  const ref = useRef<HTMLPreElement>(null);
  const stick = useRef(true);
  useLayoutEffect(() => {
    const el = ref.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [text]);
  if (text === undefined) return <div className="log log-empty">Loading log…</div>;
  if (text === "") {
    return <div className="log log-empty">{status === "running" ? "Waiting for output…" : status === "queued" ? "Not started yet." : "No output."}</div>;
  }
  return (
    <pre
      ref={ref}
      className="log"
      onScroll={(e) => {
        const el = e.currentTarget;
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
      }}
    >
      {text.replace(ANSI, "")}
    </pre>
  );
}

interface Scale {
  start: number;
  span: number;
}

function Bar({ task, scale, now }: { task: TaskView; scale: Scale; now: number }) {
  if (task.startedAt == null) return <div className="track" />;
  const left = Math.max(0, Math.min(1, (task.startedAt - scale.start) / scale.span));
  if (task.status === "cached") {
    return (
      <div className="track">
        <div className="bar-marker s-cached" style={{ left: `${left * 100}%` }} title="Cache hit" />
      </div>
    );
  }
  const dur = task.status === "running" ? now - task.startedAt : (task.durationMs ?? (task.finishedAt ? task.finishedAt - task.startedAt : 0));
  const width = Math.max(0, Math.min(1 - left, dur / scale.span));
  return (
    <div className="track">
      <div className={`bar s-bar-${task.status}`} style={{ left: `${left * 100}%`, width: `max(${width * 100}%, 3px)` }} />
    </div>
  );
}

function TaskRow({
  task,
  scale,
  now,
  open,
  onToggle,
  log,
}: {
  task: TaskView;
  scale: Scale;
  now: number;
  open: boolean;
  onToggle: () => void;
  log: string | undefined;
}) {
  const dur =
    task.status === "running" && task.startedAt
      ? now - task.startedAt
      : task.status === "cached"
        ? null
        : (task.durationMs ?? (task.startedAt && task.finishedAt ? task.finishedAt - task.startedAt : null));
  return (
    <div className={`task${open ? " open" : ""}`}>
      <button className="task-row" onClick={onToggle} aria-expanded={open}>
        <span className="task-name">
          <svg className="chev" width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M3 2l4 3-4 3" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
          <StatusIcon status={task.status} />
          <span className="name">{task.name}</span>
          {task.message && <span className="task-msg">{task.message}</span>}
        </span>
        <Bar task={task} scale={scale} now={now} />
        <span className={`task-dur mono s-text-${task.status}`}>
          {task.status === "cached" ? "cached" : task.status === "skipped" ? "skipped" : task.status === "queued" ? "queued" : fmtDuration(dur)}
        </span>
      </button>
      {open && (
        <div className="task-detail">
          <div className="task-facts">
            {task.deps.length > 0 && (
              <span>
                needs{" "}
                {task.deps.map((d) => (
                  <Chip key={d}>{d}</Chip>
                ))}
              </span>
            )}
            {task.exitCode != null && <span>exit {task.exitCode}</span>}
            {task.actionKey && (
              <span className="mono" title={task.actionKey}>
                key {task.actionKey.slice(0, 12)}
              </span>
            )}
          </div>
          <LogPanel text={log} status={task.status} />
        </div>
      )}
    </div>
  );
}

function GroupCard({
  group,
  scale,
  now,
  isOpen,
  toggle,
  logs,
}: {
  group: GroupView;
  scale: Scale;
  now: number;
  isOpen: (t: TaskView) => boolean;
  toggle: (t: TaskView) => void;
  logs: Record<string, string>;
}) {
  const prepLeft = group.startedAt != null ? Math.max(0, (group.startedAt - scale.start) / scale.span) : 0;
  const prepWidth = group.prepareMs ? Math.min(1 - prepLeft, group.prepareMs / scale.span) : 0;
  return (
    <section className={`card group s-card-${group.status}`}>
      <header className="group-head">
        <div className="group-title">
          <StatusIcon status={group.status} size={16} />
          <span className="group-name">{group.name}</span>
          <Chip>{group.os}</Chip>
          {group.worker && (
            <span className="muted">
              on <span className="mono">{group.worker}</span>
            </span>
          )}
        </div>
        <div className="group-meta muted">
          {group.prepareMs != null ? (
            <span>
              workspace ready in <span className="mono">{fmtDuration(group.prepareMs)}</span>
            </span>
          ) : group.status === "queued" ? (
            <span>waiting for a worker</span>
          ) : group.status === "running" && group.tasks.every((t) => t.status === "queued") ? (
            <span>preparing workspace…</span>
          ) : null}
        </div>
      </header>
      {group.message && <div className="group-msg">{group.message}</div>}
      <div className="tasks">
        <div className="axis">
          <span />
          <div className="track axis-track">
            {prepWidth > 0 && (
              <div className="bar-prep" style={{ left: `${prepLeft * 100}%`, width: `max(${prepWidth * 100}%, 2px)` }} title={`Workspace prepare ${fmtDuration(group.prepareMs)}`} />
            )}
            <span className="axis-l">0s</span>
            <span className="axis-r">{fmtDuration(scale.span)}</span>
          </div>
          <span />
        </div>
        {group.tasks.length === 0 && <div className="muted pad">No tasks in this group.</div>}
        {group.tasks.map((t) => (
          <TaskRow key={t.id} task={t} scale={scale} now={now} open={isOpen(t)} onToggle={() => toggle(t)} log={logs[t.id]} />
        ))}
      </div>
    </section>
  );
}

export function RunPage({ id }: { id: string }) {
  const [run, setRun] = useState<RunView>();
  const [error, setError] = useState<Error>();
  const [logs, setLogs] = useState<Record<string, string>>({});
  const [overrides, setOverrides] = useState<Record<string, boolean>>({});
  const [cancelling, setCancelling] = useState(false);
  const fetching = useRef(new Set<string>());
  const runRef = useRef<RunView>(undefined);
  runRef.current = run;

  useEffect(() => {
    let closed = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const refetch = (delay = 150) => {
      clearTimeout(timer);
      timer = setTimeout(() => {
        api.run(id).then(
          (r) => !closed && setRun(r),
          () => {},
        );
      }, delay);
    };
    const es = eventSource(`/api/runs/${encodeURIComponent(id)}/events`);
    es.onmessage = (m) => {
      let e: PitEvent;
      try {
        e = JSON.parse(m.data);
      } catch {
        return;
      }
      const now = Date.now();
      switch (e.kind) {
        case "snapshot":
          setRun(e.data as RunView);
          setError(undefined);
          if (done((e.data as RunView).status)) es.close();
          break;
        case "task.log":
          if (e.taskId) {
            const tid = e.taskId;
            setLogs((prev) => (prev[tid] === undefined ? prev : { ...prev, [tid]: prev[tid] + (e.data?.text ?? "") }));
          }
          break;
        case "task.started":
          setRun((r) => patchTask(r, e.taskId, { status: "running", startedAt: now }));
          if (e.taskId) {
            const tid = e.taskId;
            setLogs((prev) => (prev[tid] === undefined ? prev : { ...prev, [tid]: "" }));
          }
          refetch();
          break;
        case "task.finished":
          setRun((r) =>
            patchTask(r, e.taskId, {
              status: e.data?.status,
              durationMs: e.data?.durationMs ?? null,
              exitCode: e.data?.exitCode ?? null,
              finishedAt: now,
            }),
          );
          refetch();
          break;
        case "group.started":
          setRun((r) => patchGroup(r, e.groupId, { status: "running", worker: e.data?.worker ?? null, startedAt: now }));
          refetch();
          break;
        case "run.finished":
          es.close();
          refetch(0);
          break;
        default:
          refetch();
      }
    };
    es.onerror = () => {
      if (closed || runRef.current) return;
      // No snapshot yet: find out why (404, 401, server down).
      api.run(id).then(
        (r) => !closed && setRun(r),
        (err: Error) => {
          if (closed) return;
          setError(err);
          es.close();
        },
      );
    };
    return () => {
      closed = true;
      clearTimeout(timer);
      es.close();
    };
  }, [id]);

  const isOpen = (t: TaskView) => overrides[t.id] ?? (t.status === "failed" || t.status === "error");
  const toggle = (t: TaskView) => setOverrides((o) => ({ ...o, [t.id]: !isOpen(t) }));

  // Load logs for expanded tasks.
  useEffect(() => {
    if (!run) return;
    for (const t of run.groups.flatMap((g) => g.tasks)) {
      if (!isOpen(t) || logs[t.id] !== undefined || fetching.current.has(t.id)) continue;
      if (t.status === "queued") {
        setLogs((prev) => ({ ...prev, [t.id]: "" }));
        continue;
      }
      fetching.current.add(t.id);
      api.taskLog(t.id).then(
        (text) => setLogs((prev) => ({ ...prev, [t.id]: text })),
        (err: Error) => setLogs((prev) => ({ ...prev, [t.id]: `Could not load log: ${err.message}` })),
      ).finally(() => fetching.current.delete(t.id));
    }
  }, [run, overrides]);

  const active = !!run && isActive(run.status);
  const now = useNow(active ? 250 : 30000, true);

  if (error) {
    return (
      <div className="page">
        <ErrorBox error={error} />
        <Link to="/">Back to runs</Link>
      </div>
    );
  }
  if (!run) return <div className="page"><Loading /></div>;

  const start = run.startedAt ?? run.createdAt;
  const ends = run.groups.flatMap((g) => [g.finishedAt ?? 0, ...g.tasks.map((t) => t.finishedAt ?? 0)]);
  const end = run.finishedAt ?? (active ? now : Math.max(start, ...ends));
  const scale: Scale = { start, span: Math.max(end - start, 1) };
  const total = runDuration(run, now);

  const cancel = async () => {
    setCancelling(true);
    try {
      await api.cancel(run.id);
      setRun(await api.run(run.id));
    } catch (e) {
      alert(`Cancel failed: ${(e as Error).message}`);
    } finally {
      setCancelling(false);
    }
  };

  return (
    <div className="page">
      <div className="crumbs">
        <Link to="/">Runs</Link>
        <span>/</span>
        <span className="mono">{run.id}</span>
      </div>
      <div className={`run-head card s-card-${run.status}`}>
        <div className="run-head-top">
          <div className="run-head-title">
            <StatusBadge status={run.status} />
            <h1>
              {run.repo}
              <span className="at">@</span>
              <span className="mono" title={run.sha}>
                {shortSha(run.sha)}
              </span>
            </h1>
          </div>
          {active && (
            <button className="btn btn-danger" onClick={cancel} disabled={cancelling}>
              {cancelling ? "Cancelling…" : "Cancel run"}
            </button>
          )}
        </div>
        <dl className="facts">
          {run.ref && (
            <div>
              <dt>Ref</dt>
              <dd className="mono">{run.ref}</dd>
            </div>
          )}
          <div>
            <dt>Trigger</dt>
            <dd>
              <Chip>{run.trigger}</Chip>
            </dd>
          </div>
          <div>
            <dt>Total</dt>
            <dd className={`mono${active ? " live" : ""}`}>{fmtDuration(total)}</dd>
          </div>
          <div>
            <dt>Tasks</dt>
            <dd>{taskSummary(run)}</dd>
          </div>
          {run.overlayFiles > 0 && (
            <div>
              <dt>Overlay</dt>
              <dd>
                +{run.overlayFiles} local file{run.overlayFiles === 1 ? "" : "s"}
              </dd>
            </div>
          )}
          <div>
            <dt>Created</dt>
            <dd title={fmtTime(run.createdAt)}>{fmtAgo(run.createdAt, Date.now())}</dd>
          </div>
        </dl>
        {run.message && <div className="run-msg">{run.message}</div>}
      </div>
      {run.groups.length === 0 ? (
        <Empty title={run.status === "queued" ? "Planning…" : "No groups"}>
          {run.status === "queued" ? "Waiting for the config to be evaluated and groups to be planned." : "This run planned no groups."}
        </Empty>
      ) : (
        <div className="groups">
          {run.groups.map((g) => (
            <GroupCard key={g.id} group={g} scale={scale} now={now} isOpen={isOpen} toggle={toggle} logs={logs} />
          ))}
        </div>
      )}
    </div>
  );
}
