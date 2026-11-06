mod config;
mod db;
mod key;
mod proxy;
mod quota;
mod report;
mod tui;

use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rusqlite::Connection;
use time::{Date, Month, OffsetDateTime};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::report::{Agg, Group, Prices, Table};

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
    Report {
        #[arg(long, default_value_t = 30)]
        days: i64,
        /// Calendar month in YYYY-MM form. Overrides --days.
        #[arg(long)]
        month: Option<String>,
        #[arg(long)]
        csv: bool,
    },
    Config,
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
}

#[derive(Subcommand)]
enum KeyCommand {
    Add {
        #[arg(long)]
        device: String,
        #[arg(long)]
        note: Option<String>,
        /// Monthly token cap (`100M`, `1500000`, `unlimited`). Asked on a terminal
        /// when every cap is omitted.
        #[arg(long, alias = "limit-monthly")]
        limit: Option<String>,
        /// Weekly token cap over the trailing 7 days.
        #[arg(long, alias = "limit-week")]
        weekly: Option<String>,
        /// Token cap over the trailing 5 hours.
        #[arg(long = "limit-5h", alias = "5h")]
        five_hour: Option<String>,
        /// Lift the model allowlist for this key.
        #[arg(long)]
        all_models: bool,
        /// Store prompt and response bodies for this key until they expire.
        #[arg(long)]
        capture: bool,
    },
    List,
    /// Change the caps or the model restriction of an existing key.
    Update {
        prefix: String,
        /// New monthly cap (`100M`, `unlimited` clears it).
        #[arg(long, alias = "limit-monthly")]
        limit: Option<String>,
        /// New weekly cap.
        #[arg(long, alias = "limit-week")]
        weekly: Option<String>,
        /// New 5-hour cap.
        #[arg(long = "limit-5h", alias = "5h")]
        five_hour: Option<String>,
        /// Lift the model allowlist for this key.
        #[arg(long, conflicts_with = "restricted")]
        all_models: bool,
        /// Put the model allowlist back.
        #[arg(long)]
        restricted: bool,
    },
    Revoke {
        prefix: String,
    },
    Capture {
        prefix: String,
        /// Turn capture off instead of on.
        #[arg(long)]
        off: bool,
        /// Hours until capture switches itself off.
        #[arg(long)]
        hours: Option<i64>,
    },
}

#[derive(Subcommand)]
enum DbCommand {
    /// Delete requests older than the retention window.
    Prune {
        #[arg(long)]
        days: Option<i64>,
    },
    /// Fold requests into daily aggregates so long-term trends survive pruning.
    Rollup {
        #[arg(long, default_value_t = 365)]
        days: i64,
    },
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
        Command::Tui => run_tui(&cli.config),
        Command::Report { days, month, csv } => run_report(&cli.config, days, month.as_deref(), csv),
        Command::Config => show_config(&cli.config),
        Command::Key { command } => run_key(&cli.config, command),
        Command::Db { command } => match command {
            DbCommand::Prune { days } => run_prune(&cli.config, days),
            DbCommand::Rollup { days } => run_rollup(&cli.config, days),
        },
    }
}

async fn serve(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    if config.upstream_api_key.is_empty() {
        bail!(
            "upstream_api_key is not set; set `upstream_api_key` in {} or AI_PROXY_UPSTREAM_API_KEY",
            config_path.display()
        );
    }
    let conn = db::open(&config.db_path)?;
    prune_now(&conn, &config);

    let addr: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address: {}", config.listen))?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
        .read_timeout(Duration::from_secs(config.read_timeout_secs))
        .build()
        .context("failed to build HTTP client")?;

    tracing::info!(listen = %addr, "listening");
    tracing::info!(db_path = %config.db_path.display(), "database");
    tracing::info!(retention_days = config.retention_days, "retention");
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

fn run_tui(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let conn = db::open(&config.db_path)?;
    tui::run(conn, &config)
}

fn run_report(config_path: &Path, days: i64, month: Option<&str>, csv: bool) -> Result<()> {
    let config = Config::load(config_path)?;
    let conn = db::open(&config.db_path)?;
    let (since, until, label) = period(days, month)?;
    let prices = Prices::load(&conn)?;
    let rows = report::load_rows(&conn, since, until)?;

    if csv {
        println!("bucket,label,requests,input,cached,hit_rate,output,reasoning,cost_usd,unpriced");
        for group in [Group::Day, Group::Device, Group::Ip, Group::Model, Group::Key] {
            for agg in report::aggregate(&rows, group, &prices, group == Group::Day) {
                println!(
                    "{},{},{},{},{},{},{},{},{:.6},{}",
                    match group {
                        Group::Day => "day",
                        Group::Device => "device",
                        Group::Ip => "ip",
                        Group::Model => "model",
                        Group::Key => "key",
                    },
                    csv_escape(&agg.label),
                    agg.requests,
                    agg.input,
                    agg.cached,
                    agg.hit_rate().map(|rate| format!("{:.4}", rate)).unwrap_or_default(),
                    agg.output,
                    agg.reasoning,
                    agg.cost,
                    agg.unpriced.iter().cloned().collect::<Vec<_>>().join(";")
                );
            }
        }
        return Ok(());
    }

    let total = report::totals(&rows, &prices);
    println!("ai-proxy report · {label}");
    println!(
        "requests {} · input {} (cached {} · hit {}) · output {} · cost {}",
        report::fmt_int(total.requests),
        report::fmt_int(total.input),
        report::fmt_int(total.cached),
        report::fmt_rate(total.hit_rate()),
        report::fmt_int(total.output),
        report::fmt_cost(&total)
    );
    if !total.unpriced.is_empty() {
        println!(
            "cost is a lower bound: no price row for {}",
            total.unpriced.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    println!(
        "cache saved compared with full input price: {}",
        report::fmt_usd(total.saved)
    );
    if total.reasoning > 0 {
        println!(
            "reasoning tokens inside output: {}",
            report::fmt_int(total.reasoning)
        );
    }
    println!();

    if rows.is_empty() {
        println!("no requests recorded in this period");
        return Ok(());
    }

    for (group, reverse) in [
        (Group::Day, true),
        (Group::Device, false),
        (Group::Ip, false),
        (Group::Model, false),
        (Group::Key, false),
    ] {
        let converted = report::aggregate(&rows, group, &prices, reverse);
        if converted.is_empty() {
            continue;
        }
        println!("{}", table_for(group, &converted).render());
    }

    let unpriced = report::unpriced_models(&rows, &prices);
    if !unpriced.is_empty() {
        println!(
            "* cost excludes models with no price row: {}",
            unpriced.join(", ")
        );
        println!(
            "  add one: INSERT INTO price VALUES ('<model>', 'YYYY-MM-DD', input, cached, cache_write, output);"
        );
    }
    Ok(())
}

fn table_for(group: Group, aggs: &[Agg]) -> Table {
    let mut rows: Vec<Vec<String>> = aggs.iter().map(report::agg_row).collect();
    let total = report::fold(aggs);
    rows.push(report::agg_row(&total));
    Table {
        headers: vec![
            group.label().to_string(),
            "reqs".to_string(),
            "input".to_string(),
            "cached".to_string(),
            "hit".to_string(),
            "output".to_string(),
            "cost(USD)".to_string(),
        ],
        rows,
        right_align: vec![false, true, true, true, true, true, true],
    }
}

fn show_config(config_path: &Path) -> Result<()> {
    let settings = Config::describe(config_path)?;
    let table = Table {
        headers: vec![
            "setting".to_string(),
            "value".to_string(),
            "source".to_string(),
        ],
        rows: settings
            .iter()
            .map(|setting| vec![setting.name.clone(), setting.value.clone(), setting.source.to_string()])
            .collect(),
        right_align: vec![false, false, false],
    };
    println!("{}", table.render());
    println!();
    println!("source `default` means neither the file nor an AI_PROXY_* variable set it.");
    Ok(())
}

fn run_prune(config_path: &Path, days: Option<i64>) -> Result<()> {
    let config = Config::load(config_path)?;
    let conn = db::open(&config.db_path)?;
    let days = days.unwrap_or(config.retention_days);
    if days < 1 {
        bail!("--days must be at least 1");
    }
    let cutoff = now_ms() - days * 86_400_000;
    let removed = db::prune_requests(&conn, cutoff)?;
    let captures = db::prune_captures(&conn, now_ms())?;
    conn.execute_batch("PRAGMA incremental_vacuum;")
        .context("failed to vacuum the database")?;
    println!("deleted {removed} row(s) older than {days} day(s) and {captures} expired capture(s)");
    Ok(())
}

fn run_rollup(config_path: &Path, days: i64) -> Result<()> {
    let config = Config::load(config_path)?;
    let conn = db::open(&config.db_path)?;
    if days < 1 {
        bail!("--days must be at least 1");
    }
    let since = now_ms() - days * 86_400_000;
    let rows = db::rollup_range(&conn, since)?;
    println!("wrote {rows} daily aggregate row(s)");
    Ok(())
}

fn prune_now(conn: &Connection, config: &Config) {
    let cutoff = now_ms() - config.retention_days * 86_400_000;
    match db::prune_requests(conn, cutoff) {
        Ok(removed) if removed > 0 => tracing::info!(removed, "pruned requests"),
        Ok(_) => {}
        Err(err) => tracing::error!(error = %err, "failed to prune requests"),
    }
    match db::prune_captures(conn, now_ms()) {
        Ok(0) => {}
        Ok(removed) => tracing::info!(removed, "pruned expired captures"),
        Err(err) => tracing::error!(error = %err, "failed to prune captures"),
    }
    if let Err(err) = conn.execute_batch("PRAGMA incremental_vacuum;") {
        tracing::error!(error = %err, "failed to vacuum");
    }
}

fn period(days: i64, month: Option<&str>) -> Result<(i64, i64, String)> {
    let now = OffsetDateTime::now_utc();
    match month {
        Some(value) => {
            let (year, month_number) = parse_month(value)?;
            let start = Date::from_calendar_date(year, month_number, 1)
                .with_context(|| format!("invalid month {value}"))?;
            let (next_year, next_month) = match month_number {
                Month::December => (year + 1, Month::January),
                month => (year, month.next()),
            };
            let end = Date::from_calendar_date(next_year, next_month, 1)
                .with_context(|| format!("invalid month {value}"))?;
            Ok((
                to_ms(start.midnight().assume_utc()),
                to_ms(end.midnight().assume_utc()),
                value.to_string(),
            ))
        }
        None => {
            if days < 1 {
                bail!("--days must be at least 1");
            }
            let since = now - time::Duration::days(days);
            Ok((
                now_ms() - days * 86_400_000,
                i64::MAX,
                format!(
                    "last {days} day(s) · {} → {}",
                    since.date(),
                    now.date()
                ),
            ))
        }
    }
}

fn parse_month(value: &str) -> Result<(i32, Month)> {
    let (year, month) = value
        .split_once('-')
        .with_context(|| format!("expected YYYY-MM, got `{value}`"))?;
    let year: i32 = year.parse().with_context(|| format!("invalid year in `{value}`"))?;
    let month: u8 = month
        .parse()
        .with_context(|| format!("invalid month in `{value}`"))?;
    let month = Month::try_from(month).with_context(|| format!("invalid month in `{value}`"))?;
    Ok((year, month))
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn run_key(config_path: &Path, command: KeyCommand) -> Result<()> {
    let config = Config::load(config_path)?;
    eprintln!(
        "config {} -> db {}",
        config_path.display(),
        config.db_path.display()
    );
    let conn = db::open(&config.db_path)?;
    match command {
        KeyCommand::Add {
            device,
            note,
            limit,
            weekly,
            five_hour,
            all_models,
            capture,
        } => {
            let caps = resolve_limits(limit, weekly, five_hour, config.default_monthly_cap)?;
            let capture_until = capture.then(|| now_ms() + config.capture_ttl_hours * 3_600_000);
            add_key(
                &conn,
                &device,
                note.as_deref(),
                caps,
                all_models,
                capture_until,
            )
        }
        KeyCommand::List => list_keys(&conn),
        KeyCommand::Update {
            prefix,
            limit,
            weekly,
            five_hour,
            all_models,
            restricted,
        } => {
            let limits = db::KeyLimits {
                monthly: limit.map(|raw| parse_limit(&raw)).transpose()?,
                weekly: weekly.map(|raw| parse_limit(&raw)).transpose()?,
                five_hour: five_hour.map(|raw| parse_limit(&raw)).transpose()?,
            };
            let allow_all_models = if all_models {
                Some(true)
            } else if restricted {
                Some(false)
            } else {
                None
            };
            if limits.is_empty() && allow_all_models.is_none() {
                bail!("nothing to change: pass --limit, --weekly, --limit-5h or --all-models/--restricted");
            }
            let changed = db::set_key_settings(&conn, &prefix, limits, allow_all_models)?;
            if changed == 0 {
                bail!("no key matched prefix `{prefix}`");
            }
            println!("updated {changed} key(s)");
            Ok(())
        }
        KeyCommand::Revoke { prefix } => revoke_key(&conn, &prefix),
        KeyCommand::Capture {
            prefix,
            off,
            hours,
        } => {
            let until = if off {
                None
            } else {
                let hours = hours.unwrap_or(config.capture_ttl_hours);
                Some(now_ms() + hours * 3_600_000)
            };
            let changed = db::set_capture(&conn, &prefix, until)?;
            if changed == 0 {
                bail!("no key matched prefix `{prefix}`");
            }
            match until {
                Some(until) => println!(
                    "capture on for {changed} key(s) until {}",
                    format_ms(until)
                ),
                None => println!("capture off for {changed} key(s)"),
            }
            Ok(())
        }
    }
}

fn add_key(
    conn: &Connection,
    device: &str,
    note: Option<&str>,
    caps: Caps,
    all_models: bool,
    capture_until: Option<i64>,
) -> Result<()> {
    let now = now_ms();
    let device_id = db::ensure_device(conn, device, note, now)?;
    let generated = key::generate()?;
    let id = db::insert_api_key(
        conn,
        db::NewApiKey {
            device_id,
            hash: &generated.hash,
            prefix: &generated.prefix,
            created_at: now,
            monthly_token_limit: caps.monthly,
            weekly_token_limit: caps.weekly,
            five_hour_token_limit: caps.five_hour,
            allow_all_models: all_models,
            capture_until,
        },
    )?;
    println!("id: {id}");
    println!("prefix: {}", generated.prefix);
    println!("key: {}", generated.full);
    println!("limit monthly: {}", fmt_limit(caps.monthly));
    println!("limit weekly: {}", fmt_limit(caps.weekly));
    println!("limit 5h: {}", fmt_limit(caps.five_hour));
    println!("models: {}", if all_models { "all" } else { "allowlist" });
    if let Some(until) = capture_until {
        println!("capture: on until {}", format_ms(until));
    }
    Ok(())
}

/// Token caps for a new key.
#[derive(Clone, Copy)]
struct Caps {
    monthly: Option<i64>,
    weekly: Option<i64>,
    five_hour: Option<i64>,
}

/// Weekly cap suggested for a month: a month holds about four weeks.
const WEEKLY_PER_MONTH: i64 = 4;
/// 5-hour cap suggested for a week: a week holds about ten 5-hour windows.
const FIVE_HOUR_PER_WEEK: i64 = 10;

/// Resolves the three caps. With no flags on a terminal this asks for them,
/// taking a recommendation on an empty answer; everywhere else the flags win
/// and omissions mean no cap.
fn resolve_limits(
    monthly: Option<String>,
    weekly: Option<String>,
    five_hour: Option<String>,
    recommended_monthly: i64,
) -> Result<Caps> {
    let any_flag = monthly.is_some() || weekly.is_some() || five_hour.is_some();
    let mut caps = Caps {
        monthly: monthly.map(|raw| parse_limit(&raw)).transpose()?.flatten(),
        weekly: weekly.map(|raw| parse_limit(&raw)).transpose()?.flatten(),
        five_hour: five_hour.map(|raw| parse_limit(&raw)).transpose()?.flatten(),
    };
    if any_flag || !io::stdin().is_terminal() {
        return Ok(caps);
    }

    let recommended = match recommended_monthly {
        value if value > 0 => Some(value),
        _ => None,
    };
    eprintln!("token limits — Enter takes the recommendation, `unlimited` removes the cap");
    caps.monthly = ask_limit("monthly", recommended)?;
    let weekly_default = caps.monthly.map(|monthly| nice(monthly / WEEKLY_PER_MONTH));
    caps.weekly = ask_limit("weekly", weekly_default)?;
    let five_hour_default = caps
        .weekly
        .map(|weekly| nice(weekly / FIVE_HOUR_PER_WEEK))
        .or_else(|| caps.monthly.map(|monthly| nice(monthly / 40)));
    caps.five_hour = ask_limit("5-hour", five_hour_default)?;
    eprintln!(
        "monthly {}, weekly {}, 5h {}",
        fmt_limit(caps.monthly),
        fmt_limit(caps.weekly),
        fmt_limit(caps.five_hour)
    );
    Ok(caps)
}

fn ask_limit(label: &str, default: Option<i64>) -> Result<Option<i64>> {
    let default_text = fmt_short(default);
    eprint!("{label:>8} [{default_text}]: ");
    io::stderr().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        bail!("input ended while asking for the {label} cap");
    }
    let text = line.trim();
    if text.is_empty() {
        return Ok(default);
    }
    parse_limit(text)
}

/// Rounds to two significant digits so derived caps read like round numbers.
fn nice(value: i64) -> i64 {
    if value < 100 {
        return value;
    }
    let digits = (value as f64).log10().floor() as u32;
    let magnitude = 10i64.pow(digits.saturating_sub(1));
    ((value as f64 / magnitude as f64).round() as i64) * magnitude
}

/// Parses a token cap. Accepts digits, `.`, `_`/`,` separators and a k/m/b
/// suffix (`100M` = 100 million, `1.5M` = 1.5 million). `unlimited`, `none`
/// and `-` clear the cap.
fn parse_limit(raw: &str) -> Result<Option<i64>> {
    let text = raw.trim().to_ascii_lowercase().replace(['_', ','], "");
    if matches!(text.as_str(), "unlimited" | "none" | "-") {
        return Ok(None);
    }
    let (digits, scale) = if let Some(rest) = text.strip_suffix('k') {
        (rest, 1_000f64)
    } else if let Some(rest) = text.strip_suffix('m') {
        (rest, 1_000_000f64)
    } else if let Some(rest) = text.strip_suffix('b') {
        (rest, 1_000_000_000f64)
    } else if let Some(rest) = text.strip_suffix('g') {
        (rest, 1_000_000_000f64)
    } else {
        (text.as_str(), 1f64)
    };
    let value: f64 = digits
        .parse()
        .with_context(|| format!("invalid limit `{raw}`; try 100M, 1.5M, 1500000 or unlimited"))?;
    if !value.is_finite() || value <= 0.0 {
        bail!("limit must be greater than zero (got `{raw}`)");
    }
    let scaled = value * scale;
    if scaled > i64::MAX as f64 {
        bail!("limit `{raw}` is too large");
    }
    Ok(Some(scaled.round() as i64))
}

fn fmt_limit(limit: Option<i64>) -> String {
    match limit {
        Some(value) => report::fmt_int(value),
        None => "unlimited".to_string(),
    }
}

/// Compact form for prompts and recommendations: `60M`, `1.5M`, `250K`.
fn fmt_short(limit: Option<i64>) -> String {
    let Some(value) = limit else {
        return "unlimited".to_string();
    };
    if value % 1_000_000 == 0 {
        format!("{}M", value / 1_000_000)
    } else if value >= 1_000_000 && value % 1_000 == 0 {
        format!("{:.1}M", value as f64 / 1_000_000f64)
    } else if value >= 1_000 && value % 1_000 == 0 {
        format!("{}K", value / 1_000)
    } else {
        value.to_string()
    }
}

fn list_keys(conn: &Connection) -> Result<()> {
    let keys = db::list_keys(conn)?;
    let headers = [
        "id", "device", "prefix", "created", "revoked", "monthly", "weekly", "5h", "models",
        "capture",
    ];
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
            fmt_limit(key.monthly_token_limit),
            fmt_limit(key.weekly_token_limit),
            fmt_limit(key.five_hour_token_limit),
            if key.allow_all_models {
                "all".to_string()
            } else {
                "listed".to_string()
            },
            key.capture_until
                .map(format_ms)
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
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

fn to_ms(value: OffsetDateTime) -> i64 {
    (value.unix_timestamp_nanos() / 1_000_000) as i64
}

fn format_ms(ms: i64) -> String {
    match OffsetDateTime::from_unix_timestamp(ms / 1000) {
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
