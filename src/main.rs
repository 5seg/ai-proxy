mod config;
mod db;
mod key;
mod proxy;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rusqlite::Connection;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[derive(Parser)]
#[command(name = "ai-proxy", version)]
struct Cli {
    #[arg(long, global = true, default_value = "./ai-proxy.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    Tui,
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    Export {
        #[arg(long)]
        csv: bool,
    },
}

#[derive(Subcommand)]
enum KeyCommand {
    Add {
        #[arg(long)]
        device: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
        #[arg(long)]
        capture: bool,
    },
    List,
    Revoke {
        prefix: String,
    },
}

#[derive(Subcommand)]
enum DbCommand {
    Prune,
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Serve => serve(&cli.config).await,
        Command::Tui => bail!("not implemented in this build"),
        Command::Key { command } => run_key(&cli.config, command),
        Command::Db { command } => match command {
            DbCommand::Prune => bail!("not implemented in this build"),
        },
        Command::Export { .. } => bail!("not implemented in this build"),
    }
}

async fn serve(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let conn = db::open(&config.db_path)?;
    let addr: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address: {}", config.listen))?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    tracing::info!(listen = %addr, "listening");
    tracing::info!(db_path = %config.db_path.display(), "database");
    for model in &config.models {
        tracing::info!(model = %model.id, "allowed model");
    }

    let app = proxy::router(config, conn, client);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("server failed")
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        tracing::error!(error = %err, "failed to listen for shutdown signal");
    }
}

fn run_key(config_path: &Path, command: KeyCommand) -> Result<()> {
    let config = Config::load(config_path)?;
    let conn = db::open(&config.db_path)?;
    match command {
        KeyCommand::Add {
            device,
            note,
            limit,
            capture,
        } => add_key(&conn, &device, note.as_deref(), limit, capture),
        KeyCommand::List => list_keys(&conn),
        KeyCommand::Revoke { prefix } => revoke_key(&conn, &prefix),
    }
}

fn add_key(
    conn: &Connection,
    device: &str,
    note: Option<&str>,
    limit: Option<i64>,
    capture: bool,
) -> Result<()> {
    let now = now_ms();
    let device_id = db::ensure_device(conn, device, note, now)?;
    let generated = key::generate()?;
    let id = db::insert_api_key(
        conn,
        device_id,
        &generated.hash,
        &generated.prefix,
        now,
        limit,
        capture,
    )?;
    println!("id: {id}");
    println!("prefix: {}", generated.prefix);
    println!("key: {}", generated.full);
    Ok(())
}

fn list_keys(conn: &Connection) -> Result<()> {
    let keys = db::list_keys(conn)?;
    let headers = ["id", "device", "prefix", "created", "revoked", "limit"];
    let mut rows = Vec::with_capacity(keys.len());
    for key in &keys {
        rows.push([
            key.id.to_string(),
            key.device.clone(),
            key.prefix.clone(),
            format_ms(key.created_at),
            key.revoked_at
                .map(format_ms)
                .unwrap_or_else(|| "-".to_string()),
            key.monthly_token_limit
                .map(|limit| limit.to_string())
                .unwrap_or_else(|| "-".to_string()),
        ]);
    }

    let mut widths = headers.map(str::len);
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }

    let header_line = headers
        .iter()
        .enumerate()
        .map(|(index, header)| format!("{header:<width$}", width = widths[index]))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{header_line}");
    for row in &rows {
        let line = row
            .iter()
            .enumerate()
            .map(|(index, cell)| format!("{cell:<width$}", width = widths[index]))
            .collect::<Vec<_>>()
            .join("  ");
        println!("{line}");
    }
    Ok(())
}

fn revoke_key(conn: &Connection, prefix: &str) -> Result<()> {
    let changed = db::revoke_keys(conn, prefix, now_ms())?;
    if changed == 0 {
        bail!("no active key matched prefix `{prefix}`");
    }
    println!("revoked {changed} key(s)");
    Ok(())
}

pub(crate) fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

fn format_ms(ms: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(ms / 1000) {
        Ok(value) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            value.year(),
            value.month() as u8,
            value.day(),
            value.hour(),
            value.minute(),
            value.second()
        ),
        Err(_) => ms.to_string(),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
