//! A group's service containers (`services`: a database, a cache).
//!
//! Started before the group's first task, each on a network of its own, and
//! removed after its last task whatever happened, so nothing leaks into the
//! next job. Ports are published on random host ports: two groups on one
//! worker that both want Postgres on 5432 never collide. Tasks find a
//! service through `PIT_SERVICE_<NAME>_HOST` and `_PORT`: its name and
//! container port from a `docker` task (on the same network), `127.0.0.1`
//! and the published port from a host or sandbox task.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use crate::model::{Backend, ServiceDef};

/// Label on every service container and network, for cleaning up after a
/// worker that died mid-job.
const LABEL: &str = "pit-worker";
/// How long a service may take to become ready.
const READY_TIMEOUT: Duration = Duration::from_secs(90);

pub struct Running {
    /// The group's network; `docker` tasks join it.
    pub network: String,
    containers: Vec<String>,
    /// `(service, container port, host port)`.
    ports: Vec<(String, u16, u16)>,
}

async fn docker(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("running docker")?;
    if !out.status.success() {
        bail!(
            "docker {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn env_name(service: &str) -> String {
    service.to_ascii_uppercase().replace('-', "_")
}

/// Starts the services and waits until each is ready. On error, whatever
/// was started is removed again.
pub async fn start(
    worker: &str,
    group_id: &str,
    services: &BTreeMap<String, ServiceDef>,
) -> Result<Running> {
    let network = format!("pit-{group_id}");
    let label = format!("{LABEL}={worker}");
    docker(&["network", "create", "--label", &label, &network]).await?;
    let mut running = Running {
        network,
        containers: vec![],
        ports: vec![],
    };
    if let Err(e) = start_all(&mut running, &label, group_id, services).await {
        running.stop().await;
        return Err(e);
    }
    Ok(running)
}

async fn start_all(
    running: &mut Running,
    label: &str,
    group_id: &str,
    services: &BTreeMap<String, ServiceDef>,
) -> Result<()> {
    for (name, svc) in services {
        let container = format!("pit-{group_id}-{name}");
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            container.clone(),
            "--label".into(),
            label.into(),
            "--network".into(),
            running.network.clone(),
            "--network-alias".into(),
            name.clone(),
        ];
        for port in &svc.ports {
            args.extend(["-p".into(), format!("127.0.0.1::{port}")]);
        }
        for (k, v) in &svc.env {
            args.extend(["-e".into(), format!("{k}={v}")]);
        }
        args.push(svc.image.clone());
        args.extend(svc.command.iter().cloned());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        docker(&refs)
            .await
            .with_context(|| format!("starting service {name} ({})", svc.image))?;
        running.containers.push(container.clone());
        for &port in &svc.ports {
            // "127.0.0.1:49153"
            let mapped = docker(&["port", &container, &format!("{port}/tcp")]).await?;
            let host_port = mapped
                .lines()
                .next()
                .and_then(|l| l.rsplit(':').next())
                .and_then(|p| p.parse().ok())
                .with_context(|| format!("service {name}: no host port for {port} ({mapped:?})"))?;
            running.ports.push((name.clone(), port, host_port));
        }
        wait_ready(name, svc, &container, running).await?;
    }
    Ok(())
}

async fn wait_ready(
    name: &str,
    svc: &ServiceDef,
    container: &str,
    running: &Running,
) -> Result<()> {
    let started = Instant::now();
    // The published port accepts connections as soon as docker's proxy is
    // up, before the service listens; the container's own address on the
    // group's network does not. Docker Desktop cannot reach that address
    // from the host, so the published port is the fallback.
    let ip = docker(&[
        "inspect",
        "-f",
        "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
        container,
    ])
    .await
    .unwrap_or_default();
    let target = running
        .ports
        .iter()
        .find(|(n, _, _)| n == name)
        .map(|&(_, port, host_port)| {
            if cfg!(target_os = "linux") && !ip.is_empty() {
                (ip.clone(), port)
            } else {
                ("127.0.0.1".to_string(), host_port)
            }
        });
    loop {
        let ready = match (&svc.ready, &target) {
            (Some(cmd), _) => docker(&["exec", container, "sh", "-c", cmd]).await.is_ok(),
            (None, Some((host, port))) => tokio::time::timeout(
                Duration::from_secs(1),
                tokio::net::TcpStream::connect((host.as_str(), *port)),
            )
            .await
            .is_ok_and(|c| c.is_ok()),
            (None, None) => return Ok(()),
        };
        if ready {
            return Ok(());
        }
        let state = docker(&["inspect", "-f", "{{.State.Status}}", container])
            .await
            .unwrap_or_default();
        if started.elapsed() > READY_TIMEOUT || state == "exited" {
            let logs = Command::new("docker")
                .args(["logs", "--tail", "20", container])
                .output()
                .await
                .map(|o| {
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    )
                })
                .unwrap_or_default();
            let why = if state == "exited" {
                "exited".to_string()
            } else {
                format!("was not ready after {}s", READY_TIMEOUT.as_secs())
            };
            bail!("service {name} {why}; its last log lines:\n{}", logs.trim());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

impl Running {
    /// Where each service is, for a task on `backend`.
    pub fn env(&self, backend: Backend) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (name, port, host_port) in &self.ports {
            let n = env_name(name);
            let (host, p) = if backend == Backend::Docker {
                (name.clone(), *port)
            } else {
                ("127.0.0.1".to_string(), *host_port)
            };
            // The first port is the service's port; every port also by number.
            if seen.insert(name.clone()) {
                out.push((format!("PIT_SERVICE_{n}_HOST"), host.clone()));
                out.push((format!("PIT_SERVICE_{n}_PORT"), p.to_string()));
            }
            out.push((format!("PIT_SERVICE_{n}_PORT_{port}"), p.to_string()));
        }
        out
    }

    /// Removes the containers and the network.
    pub async fn stop(&self) {
        for c in &self.containers {
            let _ = docker(&["rm", "-f", "-v", c]).await;
        }
        let _ = docker(&["network", "rm", &self.network]).await;
    }
}

/// Removes services an earlier run of this worker left behind (it died
/// mid-job).
pub async fn clean_up_after(worker: &str) {
    let filter = format!("label={LABEL}={worker}");
    if let Ok(ids) = docker(&["ps", "-aq", "--filter", &filter]).await {
        for id in ids.lines().filter(|l| !l.is_empty()) {
            let _ = docker(&["rm", "-f", "-v", id]).await;
        }
    }
    if let Ok(ids) = docker(&["network", "ls", "-q", "--filter", &filter]).await {
        for id in ids.lines().filter(|l| !l.is_empty()) {
            let _ = docker(&["network", "rm", id]).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_find_services_by_backend() {
        let r = Running {
            network: "n".into(),
            containers: vec![],
            ports: vec![
                ("db".into(), 5432, 49153),
                ("my-cache".into(), 6379, 49200),
                ("my-cache".into(), 6380, 49201),
            ],
        };
        let docker: BTreeMap<_, _> = r.env(Backend::Docker).into_iter().collect();
        assert_eq!(docker["PIT_SERVICE_DB_HOST"], "db");
        assert_eq!(docker["PIT_SERVICE_DB_PORT"], "5432");
        let host: BTreeMap<_, _> = r.env(Backend::Host).into_iter().collect();
        assert_eq!(host["PIT_SERVICE_DB_HOST"], "127.0.0.1");
        assert_eq!(host["PIT_SERVICE_DB_PORT"], "49153");
        assert_eq!(host["PIT_SERVICE_MY_CACHE_PORT"], "49200");
        assert_eq!(host["PIT_SERVICE_MY_CACHE_PORT_6380"], "49201");
    }
}
