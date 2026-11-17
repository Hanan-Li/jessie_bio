//! `api`: the control-plane server.
//!
//! ```text
//! DATABASE_URL=postgres://jessie:jessie@localhost:5433/jessie cargo run -p api
//! ```
//!
//! On start: connect to Postgres, apply migrations, publish the node
//! library (refusing to start if a published version changed), then serve.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use api::{AppState, MIGRATOR, app, node_library};
use clap::Parser;
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
}

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

    let nodes = load_library(&args.nodes)
        .with_context(|| format!("loading the node library from {}", args.nodes.display()))?;
    let report = node_library::sync(&pool, nodes, HashAlgorithm::Sha256).await?;
    println!(
        "node library: {} published, {} unchanged",
        report.published, report.unchanged
    );

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
