use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use std::process::ExitCode;
use tikv_jemallocator::Jemalloc;
use tracing::{error, info, Level};

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// Ingest historical RPKI data from RIPE FTP into PostgreSQL.
#[derive(Parser)]
#[command(author, version, name = "wayback-pg", about, long_about = None)]
struct Cli {
    /// Path to an environment variable file.
    #[arg(long, global = true)]
    env: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Incrementally ingest daily RIPE FTP snapshots after each TAL's latest observed day.
    Update {
        /// PostgreSQL connection configuration.
        #[arg(long)]
        pg_config: String,

        /// Last date to ingest (inclusive). Defaults to yesterday (UTC).
        #[arg(short, long)]
        until: Option<NaiveDate>,

        /// Restrict ingestion to TALs: afrinic, apnic, arin, lacnic, ripencc.
        #[arg(short, long, value_delimiter = ',')]
        tal: Vec<String>,

        /// Data types to ingest: roa (roas.csv.xz) and aspa (output.json.xz).
        #[arg(long, value_delimiter = ',', default_value = "roa,aspa")]
        types: Vec<DataType>,
    },
    /// Backfill an explicit historical range from RIPE FTP into PostgreSQL.
    Backfill {
        /// PostgreSQL connection configuration.
        #[arg(long)]
        pg_config: String,

        /// First date to ingest (inclusive), format YYYY-MM-DD.
        #[arg(short, long)]
        from: NaiveDate,

        /// Last date to ingest (inclusive), format YYYY-MM-DD.
        #[arg(short, long)]
        until: NaiveDate,

        /// Restrict ingestion to TALs: afrinic, apnic, arin, lacnic, ripencc.
        #[arg(short, long, value_delimiter = ',')]
        tal: Vec<String>,

        /// Data types to ingest: roa (roas.csv.xz, from 2015-03-10) and aspa
        /// (output.json.xz, from 2023-10-11).
        #[arg(long, value_delimiter = ',', default_value = "roa,aspa")]
        types: Vec<DataType>,
    },
}

/// Data families the tool ingests. ASPA history is bounded by the day the
/// `output.json.xz` artifact itself appeared (2023-10-11).
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
enum DataType {
    Roa,
    Aspa,
}

fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();
    if let Some(env_path) = cli.env.as_deref() {
        dotenvy::from_path(env_path)?;
        info!("loaded environment variables from {env_path}");
    } else {
        dotenvy::dotenv().ok();
    }

    match cli.command {
        Command::Update {
            pg_config,
            until,
            tal,
            types,
        } => {
            for data_type in dedupe(types) {
                match data_type {
                    DataType::Roa => wayback_rpki::pg_ingest::pg_update(&pg_config, until, &tal)?,
                    DataType::Aspa => {
                        wayback_rpki::pg_aspa::pg_aspa_update(&pg_config, until, &tal)?
                    }
                }
            }
            Ok(())
        }
        Command::Backfill {
            pg_config,
            from,
            until,
            tal,
            types,
        } => {
            for data_type in dedupe(types) {
                match data_type {
                    DataType::Roa => {
                        wayback_rpki::pg_ingest::pg_backfill(&pg_config, from, until, &tal)?
                    }
                    DataType::Aspa => {
                        wayback_rpki::pg_aspa::pg_aspa_backfill(&pg_config, from, until, &tal)?
                    }
                }
            }
            Ok(())
        }
    }
}

/// Keep the requested order but run each data family once.
fn dedupe(types: Vec<DataType>) -> Vec<DataType> {
    let mut seen = Vec::new();
    for data_type in types {
        if !seen.contains(&data_type) {
            seen.push(data_type);
        }
    }
    seen
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!("{error:#}");
            ExitCode::FAILURE
        }
    }
}
