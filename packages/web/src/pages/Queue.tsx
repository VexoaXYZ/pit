import { api, type QueueEntry } from "../api";
import { Chip, Empty, ErrorBox, Link, Loading, fmtAgo, shortSha, useLive, useNow } from "../lib";

const QUEUE_ICON: Record<string, string> = {
  queued: "queued",
  testing: "running",
  passed: "passed",
  landed: "passed",
  failed: "failed",
};

function Entry({ e, now }: { e: QueueEntry; now: number }) {
  return (
    <div className={`q-entry q-${e.status}`}>
      <div className="q-top">
        <span className={`badge s-${QUEUE_ICON[e.status] ?? "queued"}`}>{e.status}</span>
        {e.position != null && <span className="muted mono">#{e.position}</span>}
        <span className="q-flow">
          <span className="repo">{e.repo}</span>
          <span className="mono src">{e.source}</span>
          <span className="arrow">→</span>
          <span className="mono">{e.target}</span>
        </span>
      </div>
      <div className="q-bottom">
        <span className="mono muted" title={e.headSha}>
          head {shortSha(e.headSha)}
        </span>
        {e.candidateSha && (
          <span className="mono muted" title={e.candidateSha}>
            candidate {shortSha(e.candidateSha)}
          </span>
        )}
        {e.landedSha && (
          <span className="mono s-text-passed" title={e.landedSha}>
            landed {shortSha(e.landedSha)}
          </span>
        )}
        {e.runId && (
          <Link to={`/runs/${e.runId}`} className="mono">
            run {e.runId.replace(/^run_/, "").slice(0, 8)} ↗
          </Link>
        )}
        <span className="q-right muted" title={new Date(e.updatedAt).toLocaleString()}>
          {fmtAgo(e.updatedAt, now)}
        </span>
      </div>
      {e.affected.length > 0 && (
        <div className="chips">
          {e.affected.map((a) => (
            <Chip key={a} tone="task">
              {a}
            </Chip>
          ))}
        </div>
      )}
      {e.reason && <div className={`q-reason${e.status === "failed" ? " s-text-failed" : ""}`}>{e.reason}</div>}
    </div>
  );
}

export function QueuePage() {
  const q = useLive<QueueEntry[]>(api.queue);
  const now = useNow(15000);
  const entries = q.data ?? [];
  const active = entries.filter((e) => e.status !== "landed" && e.status !== "failed");
  const finished = entries.filter((e) => e.status === "landed" || e.status === "failed").sort((a, b) => b.updatedAt - a.updatedAt);
  const lanes = new Map<string, QueueEntry[]>();
  for (const e of active) {
    const key = e.lane ?? "";
    if (!lanes.has(key)) lanes.set(key, []);
    lanes.get(key)!.push(e);
  }
  for (const l of lanes.values()) l.sort((a, b) => (a.position ?? 1e9) - (b.position ?? 1e9) || a.createdAt - b.createdAt);
  const laneKeys = [...lanes.keys()].sort((a, b) => (a === "" ? 1 : b === "" ? -1 : a.localeCompare(b)));

  return (
    <div className="page">
      <div className="section-head">
        <h1>Merge queue</h1>
      </div>
      <p className="muted lede">Changes with disjoint affected tasks land in parallel lanes; overlapping changes are tested speculatively in order.</p>
      {q.error && <ErrorBox error={q.error} />}
      {!q.data && !q.error && <Loading />}
      {q.data && entries.length === 0 && (
        <Empty title="The queue is empty">
          Queue a branch with <code>pit queue add &lt;branch&gt;</code>, or connect GitHub and add a pull request to the queue.
        </Empty>
      )}
      {laneKeys.length > 0 && (
        <div className="lanes">
          {laneKeys.map((k) => (
            <section key={k || "waiting"} className="card lane">
              <header className="lane-head">
                <span className="lane-name">{k ? `Lane ${k}` : "Waiting"}</span>
                <span className="muted">
                  {lanes.get(k)!.length} change{lanes.get(k)!.length === 1 ? "" : "s"}
                </span>
                {!k && <span className="muted">overlaps several lanes; waits for them to drain</span>}
              </header>
              <div className="lane-body">
                {lanes.get(k)!.map((e) => (
                  <Entry key={e.id} e={e} now={now} />
                ))}
              </div>
            </section>
          ))}
        </div>
      )}
      {finished.length > 0 && (
        <>
          <h2 className="subhead">Recently finished</h2>
          <div className="card lane-body">
            {finished.map((e) => (
              <Entry key={e.id} e={e} now={now} />
            ))}
          </div>
        </>
      )}
    </div>
  );
}
