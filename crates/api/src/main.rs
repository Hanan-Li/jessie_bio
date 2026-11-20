//! `api`: the control-plane server.
//!
//! ```text
//! DATABASE_URL=postgres://jessie:jessie@localhost:5433/jessie cargo run -p api
//! ```
//!
//! On start: connect to Postgres, apply migrations, publish the node
//! library (refusing to start if a published version changed), start the
//! reaper, then serve.
//!
//! Setup commands (DESIGN.md §4.3: agent tokens come from a setup command,
//! later an admin screen):
//!
//! ```text
//! api create-compute-env laptop --nextflow-version 26.10.5
//! api create-agent laptop --name davids-laptop     # prints the token, once
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use api::agents::{self, NewComputeEnv};
use api::{AppState, MIGRATOR, app, node_library, run_reaper};
use clap::{Parser, Subcommand};
use compiler::library::load_library;
use protocol::digest::HashAlgorithm;
use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

/// Each option can also come from the environment variable named after it,
/// the usual way to configure a server.
#[derive(Parser)]
#[command(about = "The control-plane API server.")]
struct Args {
    /// Postgres connection string.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
    /// Address to listen on.
    #[arg(long, env = "JESSIE_LISTEN", default_value = "127.0.0.1:8470")]
    listen: SocketAddr,
    /// The node library directory.
    #[arg(long, env = "JESSIE_NODES", default_value = "nodes")]
    nodes: PathBuf,
    /// Without a command, run the server.
    #[command(subcommand)]
    command: Option<Command>,
}

/// An enum of subcommands: clap derives the parsing from the variants, and
/// `match` below must handle every one.
#[derive(Subcommand)]
enum Command {
    /// Create a compute environment: somewhere jobs run.
    CreateComputeEnv {
        id: String,
        /// The Nextflow version agents in this environment run.
        #[arg(long)]
        nextflow_version: String,
        /// Stop any job that runs longer than this.
        #[arg(long)]
        time_limit_secs: Option<u64>,
    },
    /// Create an agent in a compute environment and print its token. The
    /// token is shown only this once.
    CreateAgent {
        compute_env: String,
        #[arg(long)]
        name: String,
    },
}

/// How often the reaper looks for expired leases (DESIGN.md §6: ~10 s).
const REAP_EVERY: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&args.database_url)
        .await
        .context("connecting to Postgres (is DATABASE_URL right, and the database running?)")?;
    MIGRATOR.run(&pool).await.context("applying migrations")?;

    match args.command {
        None => {}
        Some(Command::CreateComputeEnv {
            id,
            nextflow_version,
            time_limit_secs,
        }) => {
            let env = NewComputeEnv {
                id,
                nextflow_version,
                time_limit_secs,
            };
            agents::create_compute_env(&pool, &env)
                .await
                .with_context(|| format!("creating compute environment `{}`", env.id))?;
            println!("created compute environment `{}`", env.id);
            return Ok(());
        }
        Some(Command::CreateAgent { compute_env, name }) => {
            let (id, token) = agents::create_agent(&pool, &compute_env, &name)
                .await
                .with_context(|| {
                    format!("creating an agent in `{compute_env}` (does that environment exist?)")
                })?;
            println!("created agent {id} ({name}) in `{compute_env}`");
            println!("token (shown only now; give it to the agent): {token}");
            return Ok(());
        }
    }

    let nodes = load_library(&args.nodes)
        .with_context(|| format!("loading the node library from {}", args.nodes.display()))?;
    let report = node_library::sync(&pool, nodes, HashAlgorithm::Sha256).await?;
    println!(
        "node library: {} published, {} unchanged",
        report.published, report.unchanged
    );

    // `tokio::spawn` runs the reaper alongside the server, on the same
    // runtime. It gets its own handle to the pool (a cheap clone); when
    // `main` returns, the runtime shuts down and the task with it.
    tokio::spawn(run_reaper(pool.clone(), REAP_EVERY));

    let listener = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("listening on {}", args.listen))?;
    println!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app(AppState { pool }))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    println!("shut down");
    Ok(())
}

/// Ctrl-C or SIGTERM (systemd, a container runtime): stop accepting new
/// requests and let in-flight ones finish. The same lesson as the agent's.
async fn shutdown_signal() {
    let mut terminate = signal(SignalKind::terminate()).expect("installing a SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}
