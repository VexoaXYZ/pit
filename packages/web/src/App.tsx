import { useEffect, useState } from "react";
import { api, type Health } from "./api";
import { Empty, Link, usePath } from "./lib";
import { RunsPage } from "./pages/Runs";
import { RunPage } from "./pages/Run";
import { QueuePage } from "./pages/Queue";
import { WorkersPage } from "./pages/Workers";

function Logo() {
  return (
    <svg width="22" height="22" viewBox="0 0 64 64" aria-hidden="true">
      <rect width="64" height="64" rx="14" fill="#16a34a" />
      <text x="32" y="32" dy="0.35em" textAnchor="middle" fontFamily="ui-sans-serif, system-ui, -apple-system, sans-serif" fontSize="36" fontWeight="700" fill="#fff">
        P
      </text>
    </svg>
  );
}

function Nav({ path }: { path: string }) {
  const [health, setHealth] = useState<Health>();
  useEffect(() => {
    const check = () => api.health().then(setHealth, () => setHealth(undefined));
    check();
    const t = setInterval(check, 30000);
    return () => clearInterval(t);
  }, []);
  const links = [
    { to: "/", label: "Runs", active: path === "/" || path.startsWith("/runs") },
    { to: "/queue", label: "Queue", active: path.startsWith("/queue") },
    { to: "/workers", label: "Workers", active: path.startsWith("/workers") },
  ];
  return (
    <header className="nav">
      <div className="nav-inner">
        <Link to="/" className="brand">
          <Logo />
          <span>Pit</span>
        </Link>
        <nav className="nav-links">
          {links.map((l) => (
            <Link key={l.to} to={l.to} className={l.active ? "nav-link active" : "nav-link"}>
              {l.label}
            </Link>
          ))}
        </nav>
        <div className="nav-right">
          {health ? (
            <span className="health" title={`auth ${health.auth ? "on" : "off"} · github ${health.github ? "connected" : "off"}`}>
              <span className="dot dot-on" />v{health.version}
            </span>
          ) : (
            <span className="health muted">
              <span className="dot" />
              offline
            </span>
          )}
        </div>
      </div>
    </header>
  );
}

export function App() {
  const path = usePath();
  let page;
  const run = path.match(/^\/runs\/([^/]+)\/?$/);
  if (path === "/" || path === "/runs" || path === "/runs/") page = <RunsPage />;
  else if (run) page = <RunPage key={run[1]} id={decodeURIComponent(run[1])} />;
  else if (path.replace(/\/$/, "") === "/queue") page = <QueuePage />;
  else if (path.replace(/\/$/, "") === "/workers") page = <WorkersPage />;
  else
    page = (
      <Empty title="Page not found">
        <Link to="/">Back to runs</Link>
      </Empty>
    );
  return (
    <>
      <Nav path={path} />
      <main className="main">{page}</main>
    </>
  );
}
