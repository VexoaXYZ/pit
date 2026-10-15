import { useEffect } from "react";
import { api, type Worker } from "../api";
import { Chip, Empty, ErrorBox, Link, Loading, fmtAgo, useLive, useNow } from "../lib";

export function WorkersPage() {
  const w = useLive<Worker[]>(api.workers, (e) => e.kind === "group.started" || e.kind === "group.finished" || e.kind === "run.finished");
  const now = useNow(10000);
  const { reload } = w;
  // Re-poll so online/offline flips (heartbeats are not broadcast as events).
  useEffect(() => {
    const t = setInterval(reload, 15000);
    return () => clearInterval(t);
  }, [reload]);
  const workers = [...(w.data ?? [])].sort((a, b) => Number(b.online) - Number(a.online) || a.name.localeCompare(b.name));
  return (
    <div className="page">
      <div className="section-head">
        <h1>Workers</h1>
        {w.data && <span className="muted">{w.data.filter((x) => x.online).length} online</span>}
      </div>
      {w.error && <ErrorBox error={w.error} />}
      {!w.data && !w.error && <Loading />}
      {w.data && w.data.length === 0 && (
        <Empty title="No workers have connected">
          Start one with <code>pit worker --server http://HOST:7070</code>, or use <code>pit up</code>, which runs a server with a built-in
          worker.
        </Empty>
      )}
      {workers.length > 0 && (
        <div className="card table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Platform</th>
                <th className="num">CPUs</th>
                <th>Backends</th>
                <th>KVM</th>
                <th>Current run</th>
                <th className="num">Last seen</th>
              </tr>
            </thead>
            <tbody>
              {workers.map((x) => (
                <tr key={x.name} className={x.online ? "" : "offline"}>
                  <td data-label="Name">
                    <span className={`dot ${x.online ? "dot-on" : ""}`} title={x.online ? "online" : "offline"} />
                    <span className="mono strong">{x.name}</span>
                  </td>
                  <td data-label="Platform" className="mono">
                    {x.os}/{x.arch}
                  </td>
                  <td data-label="CPUs" className="num mono">
                    {x.cpus}
                  </td>
                  <td data-label="Backends">
                    <span className="chips">
                      {x.backends.map((b) => (
                        <Chip key={b}>{b}</Chip>
                      ))}
                    </span>
                  </td>
                  <td data-label="KVM">{x.kvm ? <Chip tone="ok">kvm</Chip> : <span className="muted">no</span>}</td>
                  <td data-label="Current run">
                    {x.currentRun ? (
                      <Link to={`/runs/${x.currentRun}`} className="mono">
                        {x.currentRun}
                      </Link>
                    ) : (
                      <span className="muted">idle</span>
                    )}
                  </td>
                  <td data-label="Last seen" className="num muted" title={new Date(x.lastSeen).toLocaleString()}>
                    {fmtAgo(x.lastSeen, now)}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
