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
    /// Same as `pit run --no-cache`: proves results do not depend on the cache.
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
    /// Print a task's log.
    Logs {
        run: String,
        task: String,
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
    /// Cache and run statistics.
    Stats {
        #[command(flatten)]
        conn: Conn,
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

fn default_parallelism() -> usize {
    (num_cpus::get() / 2).max(2)
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let quiet = matches!(cli.command, Command::Mcp { .. });
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PIT_LOG").unwrap_or_else(|_| "pit=info".into()),
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
        Command::Up { addr, data, slots, parallelism, token } => {
            let data = data.unwrap_or_else(default_data);
            cli::up(addr, data, slots, parallelism.unwrap_or_else(default_parallelism), token).await?;
            Ok(0)
        }
        Command::Server { addr, data, token, public_url } => {
            pit::server::serve(pit::server::ServerOpts {
                data_dir: data.unwrap_or_else(default_data).join("server"),
                addr,
                token,
                public_url,
            })
            .await?;
            Ok(0)
        }
        Command::Worker { conn, name, data, slots, parallelism } => {
            pit::worker::run(pit::worker::WorkerOpts {
                server: conn.server,
                token: conn.token,
                name,
                data_dir: data.unwrap_or_else(default_data).join("worker"),
                slots,
                parallelism: parallelism.unwrap_or_else(default_parallelism),
            })
            .await?;
            Ok(0)
        }
        Command::Run { tasks, conn, no_cache, verbose, json, dir } => {
            cli::run(&cli::client(&conn.server, conn.token)?, &dir, tasks, no_cache, verbose, json).await
        }
        Command::Verify { tasks, conn, dir } => {
            cli::run(&cli::client(&conn.server, conn.token)?, &dir, tasks, true, false, false).await
        }
        Command::Runs { conn, limit } => cli::runs(&cli::client(&conn.server, conn.token)?, limit).await,
        Command::Logs { run, task, conn } => cli::logs(&cli::client(&conn.server, conn.token)?, &run, &task).await,
        Command::Queue { command } => match command {
            QueueCmd::Add { branch, target, conn, dir } => {
                cli::queue_add(&cli::client(&conn.server, conn.token)?, &dir, &branch, target).await
            }
            QueueCmd::List { conn } => cli::queue_list(&cli::client(&conn.server, conn.token)?).await,
        },
        Command::Init { dir, force } => pit::init::init(&dir, force).await,
        Command::Import { dir, stdout, force } => pit::import::import(&dir, stdout, force),
        Command::Mcp { conn } => {
            pit::mcp::serve(cli::client(&conn.server, conn.token)?).await?;
            Ok(0)
        }
        Command::Stats { conn } => cli::stats(&cli::client(&conn.server, conn.token)?).await,
    }
}
