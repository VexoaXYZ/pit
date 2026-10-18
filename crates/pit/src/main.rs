use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use pit::cli;

const DEFAULT_SERVER: &str = "http://127.0.0.1:7070";

#[derive(Parser)]
#[command(name = "pit", version, about = "Pit: CI that is never cold.", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args, Clone)]
struct Conn {
    /// Pit server URL.
    #[arg(long, env = "PIT_SERVER", default_value = DEFAULT_SERVER, global = true)]
    server: String,
    /// API token, if the server requires one.
    #[arg(long, env = "PIT_TOKEN", global = true, hide_env_values = true)]
    token: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Start a server and a worker in one process. The quickest way to run Pit locally.
    Up {
        #[arg(long, default_value = "127.0.0.1:7070")]
        addr: std::net::SocketAddr,
        #[arg(long, env = "PIT_DATA")]
        data: Option<PathBuf>,
        /// Concurrent groups for the built-in worker.
        #[arg(long, default_value_t = 2)]
        slots: usize,
        #[arg(long)]
        parallelism: Option<usize>,
        #[arg(long, env = "PIT_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Cache size limit in GB; least recently used results are evicted
        /// past it. 0 means unlimited.
        #[arg(long, env = "PIT_CACHE_MAX_GB", default_value_t = 20.0)]
        cache_max_gb: f64,
        /// Disk budget in GB for the worker's warm workspaces; least
        /// recently used idle ones are removed past it. 0 means no limit.
        #[arg(long, env = "PIT_WORKSPACE_MAX_GB", default_value_t = 50.0)]
        workspace_max_gb: f64,
        /// Required to claim jobs. Set it on the server and its workers so that
        /// API token holders cannot act as workers and write trusted results.
        #[arg(long, env = "PIT_WORKER_TOKEN", hide_env_values = true)]
        worker_token: Option<String>,
        /// `KEY=VALUE` secrets, given only to tasks that list them in
        /// `secrets` and never to fork pull requests. Prefer it to the
        /// worker's environment, which every task inherits. Host and sandbox
        /// tasks run as the worker's user and can read the file itself.
        #[arg(long, env = "PIT_SECRETS_FILE")]
        secrets_file: Option<PathBuf>,
        /// Webhook told when main or a schedule breaks or is fixed (a Slack
        /// incoming webhook works as is).
        #[arg(long, env = "PIT_NOTIFY_URL")]
        notify_url: Option<String>,
    },
    /// Run the control plane.
    Server {
        #[arg(long, default_value = "127.0.0.1:7070")]
        addr: std::net::SocketAddr,
        #[arg(long, env = "PIT_DATA")]
        data: Option<PathBuf>,
        #[arg(long, env = "PIT_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// URL used in links (GitHub check runs, CLI output).
        #[arg(long, env = "PIT_PUBLIC_URL")]
        public_url: Option<String>,
        /// Cache size limit in GB; least recently used results are evicted
        /// past it. 0 means unlimited.
        #[arg(long, env = "PIT_CACHE_MAX_GB", default_value_t = 20.0)]
        cache_max_gb: f64,
        /// Required to claim jobs. Set it on the server and its workers so that
        /// API token holders cannot act as workers and write trusted results.
        #[arg(long, env = "PIT_WORKER_TOKEN", hide_env_values = true)]
        worker_token: Option<String>,
        /// Webhook told when main or a schedule breaks or is fixed (a Slack
        /// incoming webhook works as is).
        #[arg(long, env = "PIT_NOTIFY_URL")]
        notify_url: Option<String>,
        /// Issue OIDC tokens to tasks (`oidc` in the config), so they can get
        /// cloud credentials without stored keys. Needs PIT_TOKEN and
        /// PIT_WORKER_TOKEN, and a server host no task can read.
        #[arg(long, env = "PIT_OIDC")]
        oidc: bool,
    },
    /// Run a worker that claims groups from a server.
    Worker {
        #[command(flatten)]
        conn: Conn,
        #[arg(long, env = "PIT_WORKER_NAME")]
        name: Option<String>,
        #[arg(long, env = "PIT_DATA")]
        data: Option<PathBuf>,
        #[arg(long, default_value_t = 2)]
        slots: usize,
        #[arg(long)]
        parallelism: Option<usize>,
        /// Disk budget in GB for the worker's warm workspaces; least
        /// recently used idle ones are removed past it. 0 means no limit.
        #[arg(long, env = "PIT_WORKSPACE_MAX_GB", default_value_t = 50.0)]
        workspace_max_gb: f64,
        /// Required to claim jobs. Set it on the server and its workers so that
        /// API token holders cannot act as workers and write trusted results.
        #[arg(long, env = "PIT_WORKER_TOKEN", hide_env_values = true)]
        worker_token: Option<String>,
        /// `KEY=VALUE` secrets, given only to tasks that list them in
        /// `secrets` and never to fork pull requests. Prefer it to the
        /// worker's environment, which every task inherits. Host and sandbox
        /// tasks run as the worker's user and can read the file itself.
        #[arg(long, env = "PIT_SECRETS_FILE")]
        secrets_file: Option<PathBuf>,
    },
    /// Run tasks on the working tree, including uncommitted changes.
    Run {
        /// Tasks to run (`task` or `group.task`). Default: all.
        tasks: Vec<String>,
        #[command(flatten)]
        conn: Conn,
        /// Ignore cached results.
        #[arg(long)]
        no_cache: bool,
        /// Run in a new, empty workspace, without files earlier runs left
        /// behind (ignored build output, tool caches).
        #[arg(long)]
        fresh: bool,
        /// When a task fails, keep its workspace for 30 minutes and print a
        /// script that opens a shell in the task's environment there (its
        /// container, its variables), or runs a command given to it.
        #[arg(long)]
        debug: bool,
        /// Stream every task's log, not just failures.
        #[arg(short, long)]
        verbose: bool,
        /// Print the final run as JSON.
        #[arg(long)]
        json: bool,
        /// Repository directory.
        #[arg(short = 'C', long, default_value = ".")]
        dir: PathBuf,
    },
    /// `pit run --no-cache --fresh`: proves results depend neither on the
    /// cache nor on files earlier runs left in a warm workspace.
    Verify {
        tasks: Vec<String>,
        #[command(flatten)]
        conn: Conn,
        #[arg(short = 'C', long, default_value = ".")]
        dir: PathBuf,
    },
    /// List recent runs.
    Runs {
        #[command(flatten)]
        conn: Conn,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Tasks that passed only on a retry, or passed and failed with the same
    /// inputs.
    Flaky {
        #[command(flatten)]
        conn: Conn,
        #[arg(long, default_value_t = 14)]
        days: u32,
        /// The tests inside tasks (from their JUnit `reports`) instead of
        /// whole tasks.
        #[arg(long)]
        tests: bool,
    },
    /// Tasks whose recent passing runs take much longer than earlier ones.
    Slow {
        #[command(flatten)]
        conn: Conn,
        #[arg(long, default_value_t = 14)]
        days: u32,
    },
    /// Cancel a queued or running run; its running tasks are stopped
    /// within a few seconds.
    Cancel {
        run: String,
        #[command(flatten)]
        conn: Conn,
    },
    /// Print a task's log.
    Logs {
        run: String,
        task: String,
        #[command(flatten)]
        conn: Conn,
    },
    /// Download a run's artifacts (files its tasks' `artifacts` kept, from
    /// passed and failed runs alike) into `<out>/<group>/<task>/`.
    Artifacts {
        run: String,
        /// Only this task (`task` or `group.task`).
        #[arg(long)]
        task: Option<String>,
        #[arg(long, default_value = "pit-artifacts")]
        out: PathBuf,
        #[command(flatten)]
        conn: Conn,
    },
    /// The merge queue.
    Queue {
        #[command(subcommand)]
        command: QueueCmd,
    },
    /// Write a starter pit.config.ts for this repository.
    Init {
        #[arg(short = 'C', long, default_value = ".")]
        dir: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Convert GitHub Actions workflows into a pit.config.ts.
    Import {
        #[arg(short = 'C', long, default_value = ".")]
        dir: PathBuf,
        /// Print instead of writing pit.config.ts.
        #[arg(long)]
        stdout: bool,
        #[arg(long)]
        force: bool,
    },
    /// Model Context Protocol server over stdio, for coding agents.
    Mcp {
        #[command(flatten)]
        conn: Conn,
    },
    /// Scheduled runs (`schedules` on each repo's default branch): when each
    /// fires next and how its last run went.
    Schedules {
        #[command(subcommand)]
        command: Option<SchedulesCmd>,
        #[command(flatten)]
        conn: Conn,
    },
    /// Cache and run statistics.
    Stats {
        #[command(flatten)]
        conn: Conn,
    },
}

#[derive(Subcommand)]
enum SchedulesCmd {
    /// Start a schedule now, as its cron would (the default branch's tip),
    /// to try it without waiting.
    Run {
        name: String,
        /// The repo, when more than one has a schedule by that name.
        #[arg(long)]
        repo: Option<String>,
    },
}

#[derive(Subcommand)]
enum QueueCmd {
    /// Queue a branch to merge into its target once it passes.
    Add {
        branch: String,
        #[arg(long)]
        target: Option<String>,
        #[command(flatten)]
        conn: Conn,
        #[arg(short = 'C', long, default_value = ".")]
        dir: PathBuf,
    },
    /// Take a change out of the queue (by entry id or branch).
    Remove {
        entry: String,
        #[command(flatten)]
        conn: Conn,
        #[arg(short = 'C', long, default_value = ".")]
        dir: PathBuf,
    },
    /// Show the queue.
    List {
        #[command(flatten)]
        conn: Conn,
    },
}

fn default_data() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".pit")
}

/// `--data`, made absolute: workers run git in other directories, where a
/// relative path (`--data ../w`) names nothing.
fn data_root(data: Option<PathBuf>) -> PathBuf {
    let data = data.unwrap_or_else(default_data);
    std::path::absolute(&data).unwrap_or(data)
}

fn gb_to_bytes(gb: f64) -> u64 {
    (gb.max(0.0) * 1_000_000_000.0) as u64
}

fn default_parallelism() -> usize {
    (num_cpus::get() / 2).max(2)
}

#[tokio::main]
async fn main() {
    // Rust ignores SIGPIPE, so `pit runs | head` would panic on a closed
    // pipe. Exit quietly instead, like other command-line tools.
    let cli = Cli::parse();
    // Not for the daemons: a closed log pipe must not kill a server or worker.
    #[cfg(unix)]
    if !matches!(
        cli.command,
        Command::Up { .. } | Command::Server { .. } | Command::Worker { .. }
    ) {
        // SAFETY: restoring the default disposition of a signal, before any
        // other thread could depend on it.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    let quiet = matches!(cli.command, Command::Mcp { .. });
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PIT_LOG")
                .unwrap_or_else(|_| "pit=info".into()),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(!quiet)
        .init();
    match run(cli).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("pit: {e:#}");
            std::process::exit(2);
        }
    }
}

async fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Up {
            addr,
            data,
            slots,
            parallelism,
            token,
            cache_max_gb,
            workspace_max_gb,
            worker_token,
            notify_url,
            secrets_file,
        } => {
            let data = data_root(data);
            cli::up(cli::UpOpts {
                addr,
                data,
                slots,
                parallelism: parallelism.unwrap_or_else(default_parallelism),
                token,
                cache_max_bytes: gb_to_bytes(cache_max_gb),
                workspace_max_bytes: gb_to_bytes(workspace_max_gb),
                worker_token,
                notify_url,
                secrets_file,
            })
            .await?;
            Ok(0)
        }
        Command::Server {
            addr,
            data,
            token,
            public_url,
            cache_max_gb,
            worker_token,
            notify_url,
            oidc,
        } => {
            pit::server::serve(pit::server::ServerOpts {
                data_dir: data_root(data).join("server"),
                addr,
                token,
                public_url,
                cache_max_bytes: gb_to_bytes(cache_max_gb),
                worker_token,
                notify_url,
                oidc,
            })
            .await?;
            Ok(0)
        }
        Command::Worker {
            conn,
            name,
            data,
            slots,
            parallelism,
            workspace_max_gb,
            worker_token,
            secrets_file,
        } => {
            pit::worker::run(pit::worker::WorkerOpts {
                server: conn.server,
                token: conn.token,
                name,
                data_dir: data_root(data).join("worker"),
                slots,
                parallelism: parallelism.unwrap_or_else(default_parallelism),
                workspace_max_bytes: gb_to_bytes(workspace_max_gb),
                worker_token,
                secrets_file,
            })
            .await?;
            Ok(0)
        }
        Command::Run {
            tasks,
            conn,
            no_cache,
            fresh,
            debug,
            verbose,
            json,
            dir,
        } => {
            cli::run(
                &cli::client(&conn.server, conn.token)?,
                &dir,
                tasks,
                cli::RunOpts {
                    no_cache,
                    fresh,
                    debug,
                },
                verbose,
                json,
            )
            .await
        }
        Command::Verify { tasks, conn, dir } => {
            // No cache and no warm state: the run a brand-new runner would do.
            cli::run(
                &cli::client(&conn.server, conn.token)?,
                &dir,
                tasks,
                cli::RunOpts {
                    no_cache: true,
                    fresh: true,
                    debug: false,
                },
                false,
                false,
            )
            .await
        }
        Command::Runs { conn, limit } => {
            cli::runs(&cli::client(&conn.server, conn.token)?, limit).await
        }
        Command::Flaky { conn, days, tests } => {
            let client = cli::client(&conn.server, conn.token)?;
            if tests {
                cli::flaky_tests(&client, days).await
            } else {
                cli::flaky(&client, days).await
            }
        }
        Command::Slow { conn, days } => {
            cli::slow(&cli::client(&conn.server, conn.token)?, days).await
        }
        Command::Artifacts {
            run,
            task,
            out,
            conn,
        } => {
            cli::artifacts(
                &cli::client(&conn.server, conn.token)?,
                &run,
                task.as_deref(),
                &out,
            )
            .await
        }
        Command::Cancel { run, conn } => {
            cli::cancel(&cli::client(&conn.server, conn.token)?, &run).await
        }
        Command::Logs { run, task, conn } => {
            cli::logs(&cli::client(&conn.server, conn.token)?, &run, &task).await
        }
        Command::Queue { command } => match command {
            QueueCmd::Add {
                branch,
                target,
                conn,
                dir,
            } => {
                cli::queue_add(
                    &cli::client(&conn.server, conn.token)?,
                    &dir,
                    &branch,
                    target,
                )
                .await
            }
            QueueCmd::Remove { entry, conn, dir } => {
                cli::queue_remove(&cli::client(&conn.server, conn.token)?, &dir, &entry).await
            }
            QueueCmd::List { conn } => {
                cli::queue_list(&cli::client(&conn.server, conn.token)?).await
            }
        },
        Command::Init { dir, force } => pit::init::init(&dir, force).await,
        Command::Import { dir, stdout, force } => pit::import::import(&dir, stdout, force),
        Command::Mcp { conn } => {
            pit::mcp::serve(cli::client(&conn.server, conn.token)?).await?;
            Ok(0)
        }
        Command::Schedules { command, conn } => {
            let client = cli::client(&conn.server, conn.token)?;
            match command {
                None => cli::schedules(&client).await,
                Some(SchedulesCmd::Run { name, repo }) => {
                    cli::schedule_run(&client, &name, repo.as_deref()).await
                }
            }
        }
        Command::Stats { conn } => cli::stats(&cli::client(&conn.server, conn.token)?).await,
    }
}
