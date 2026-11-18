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
    /// Export the ROA effective-interval table as Parquet.
    ///
    /// One row per (prefix, origin, max_len, from_ts, to_ts): the intervals during
    /// which the archive shows that authorization in effect. Consumers use it to
    /// validate BGP data against the ROA state of a past day without querying
    /// PostgreSQL per file.
    ExportSpans {
        /// PostgreSQL connection configuration.
        #[arg(long)]
        pg_config: String,

        /// Destination path: a local file or an S3 URI.
        #[arg(short, long)]
        out: String,

        /// Drop intervals that end before this date (YYYY-MM-DD). Defaults to all.
        #[arg(long)]
        since: Option<NaiveDate>,
    },
    /// Build a `bgpkit-snap` object from the spans Parquet and publish it.
    ///
    /// The object is a pure function of the table `export-spans` already writes, so this
    /// never reads PostgreSQL. Publication order is: verify, upload the object, then
    /// rewrite the manifest, which is the only mutable key.
    SnapBuild {
        /// The spans Parquet, local path or S3 URI.
        #[arg(long)]
        spans: String,

        /// Where published objects live, e.g. s3://spaces/broker/snap/.
        #[arg(short, long)]
        publish: String,

        /// Publish even when the guards fire (a deliberate rebuild).
        #[arg(long)]
        force: bool,

        /// Build and verify, but do not upload anything.
        #[arg(long)]
        dry_run: bool,

        /// Daily objects to keep; older ones survive only as month ends.
        #[arg(long, default_value = "30")]
        keep_daily: usize,

        /// Also write the pre-2.0 JSONL transport here, e.g.
        /// `s3://spaces/broker/roas_trie.jsonl.gz`. Binaries from before 2.0 cannot read a
        /// snap object and bootstrap from this file, so publishing keeps it current.
        #[arg(long)]
        legacy_jsonl: Option<String>,
    },
    /// Compare a published object with PostgreSQL, prefix by prefix.
    SnapCheck {
        /// PostgreSQL connection configuration.
        #[arg(long)]
        pg_config: String,

        /// The object, local path or S3 URI.
        #[arg(long)]
        object: String,

        /// Prefixes to sample across the table.
        #[arg(long, default_value = "200")]
        sample: usize,
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
        Command::ExportSpans {
            pg_config,
            out,
            since,
        } => wayback_rpki::pg_export::export_spans(&pg_config, &out, since),
        Command::SnapBuild {
            spans,
            publish,
            force,
            dry_run,
            keep_daily,
            legacy_jsonl,
        } => snap_build(
            &spans,
            &publish,
            force,
            dry_run,
            keep_daily,
            legacy_jsonl.as_deref(),
        ),
        Command::SnapCheck {
            pg_config,
            object,
            sample,
        } => snap_check(&pg_config, &object, sample),
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

/// Build the snap object from the spans table and publish it, in that order.
fn snap_build(
    spans: &str,
    publish: &str,
    force: bool,
    dry_run: bool,
    keep_daily: usize,
    legacy_jsonl: Option<&str>,
) -> anyhow::Result<()> {
    use wayback_rpki::snap_export::{self, Decision, Manifest, Published, Target};

    // oneio reads local paths and s3:// URIs alike, so an object can be built straight from
    // the bucket that already holds the export
    let local = if spans.starts_with("s3://") {
        let temp = std::env::temp_dir().join(format!(
            "wayback-spans-{}-{}.parquet",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        oneio::download(spans, temp.to_str().unwrap())
            .map_err(|e| anyhow::anyhow!("downloading {spans}: {e}"))?;
        temp
    } else {
        std::path::PathBuf::from(spans)
    };

    let source_sha = snap_export::file_sha256(&local)?;
    let parquet_bytes = std::fs::metadata(&local)?.len();

    let target = Target::parse(publish)?;
    let manifest_url = target.manifest_url();
    let mut manifest: Manifest = snap_export::read_manifest(&manifest_url)?;
    let previous = manifest.latest().cloned();

    let built = snap_export::build_from_parquet(&local)?;
    // the guards decide whether this build replaces what the manifest names
    match snap_export::decide(
        previous.as_ref(),
        &source_sha,
        built.entries,
        built.window,
        force,
    ) {
        Decision::SkipUnchanged => {
            println!("source unchanged (sha256 {source_sha}), nothing to publish");
            if local != std::path::Path::new(spans) {
                let _ = std::fs::remove_file(&local);
            }
            return Ok(());
        }
        Decision::Refuse(message) => {
            anyhow::bail!("refusing to publish: {message}");
        }
        Decision::Publish => {}
    }

    let key = target.object_key(&built.content_id);
    let published = Published {
        key: key.clone(),
        content_id: built.content_id.clone(),
        type_id: "bgpkit.roa-set:v1".to_string(),
        bytes: built.bytes.len() as u64,
        entries: built.entries,
        v4_entries: built.v4_entries,
        v6_entries: built.v6_entries,
        records: built.records,
        window: built.window,
        built_at: chrono::Utc::now().timestamp(),
        source_sha256: source_sha.clone(),
    };

    println!(
        "{} entries (v4 {}, v6 {}), {} records, window {}..{}, {} bytes, content id {}",
        built.entries,
        built.v4_entries,
        built.v6_entries,
        built.records,
        built.window.0,
        built.window.1,
        built.bytes.len(),
        built.content_id
    );

    if dry_run {
        println!("dry run, not uploading s3://{}/{key}", target.bucket);
        if local != std::path::Path::new(spans) {
            let _ = std::fs::remove_file(&local);
        }
        return Ok(());
    }

    // verify, upload the object, then move the manifest: any failure leaves the previous
    // object and manifest in place, and an orphan object is harmless because it is
    // content addressed
    snap_export::upload(&target, &key, &built.bytes)?;
    println!("uploaded s3://{}/{key}", target.bucket);

    // the pre-2.0 transport, from the same rows: old binaries cannot read the object, and a
    // second, independent export would drift from it eventually
    if let Some(uri) = legacy_jsonl {
        // oneio::get_writer only creates local files, so an s3:// URI handed to it would
        // write a directory called "s3:" and report success. Parse the URI and use the
        // upload function that actually talks to the bucket.
        let (bucket, key) = oneio::s3_url_parse(uri)
            .map_err(|e| anyhow::anyhow!("{uri} is not an s3://bucket/key URI: {e}"))?;
        let temp = std::env::temp_dir().join(format!(
            "roas_trie-{}-{}.jsonl.gz",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let (by_prefix, _) = snap_export::group_parquet(&local)?;
        let lines = snap_export::write_jsonl(&by_prefix, temp.to_str().unwrap())?;
        let bytes = std::fs::metadata(&temp)?.len();
        oneio::s3_upload(&bucket, &key, temp.to_str().unwrap())
            .map_err(|e| anyhow::anyhow!("uploading {uri}: {e}"))?;
        let _ = std::fs::remove_file(&temp);
        // "uploaded" has to mean the bucket has it, not that a function returned Ok
        if !oneio::exists(uri).unwrap_or(false) {
            anyhow::bail!("{uri} does not exist after the upload");
        }
        println!("uploaded {uri} ({lines} rows, {bytes} bytes)");
    }

    // content addressed: publishing the same bytes again is a no-op on the object, so it is
    // a no-op on the manifest too, and repeated --force runs do not grow the list
    match manifest.objects.last_mut() {
        Some(last) if last.content_id == published.content_id => *last = published,
        _ => manifest.objects.push(published),
    }

    // retention: drop the dailies nobody needs, keeping month ends, and only after the new
    // object is in place so a failure here never shortens what the manifest names
    let drop = snap_export::plan_prune(&manifest.objects, keep_daily);
    if !drop.is_empty() {
        let mut doomed = Vec::with_capacity(drop.len());
        for index in &drop {
            doomed.push(manifest.objects[*index].key.clone());
        }
        for key in &doomed {
            snap_export::delete_object(&target, key)?;
            println!("pruned s3://{}/{key}", target.bucket);
        }
        let mut index = 0;
        manifest.objects.retain(|_| {
            let keep = !drop.contains(&index);
            index += 1;
            keep
        });
    }

    manifest.source = snap_export::Source {
        parquet_sha256: source_sha,
        parquet_bytes,
    };
    manifest.generated_at = chrono::Utc::now().timestamp();
    snap_export::write_manifest(&target, &manifest)?;
    println!("manifest updated at {manifest_url}");

    if local != std::path::Path::new(spans) {
        let _ = std::fs::remove_file(&local);
    }
    Ok(())
}

/// Compare a published object with PostgreSQL and report the differences.
fn snap_check(pg_config: &str, object: &str, sample: usize) -> anyhow::Result<()> {
    use wayback_rpki::snap_export;

    // an S3 object is fetched first: the check reads it like a consumer would
    let local = if object.starts_with("s3://") {
        let temp = std::env::temp_dir().join(format!(
            "wayback-snap-check-{}-{}.snap",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        oneio::download(object, temp.to_str().unwrap())
            .map_err(|e| anyhow::anyhow!("downloading {object}: {e}"))?;
        temp
    } else {
        std::path::PathBuf::from(object)
    };

    let report = snap_export::check_against_pg(pg_config, &local, sample)?;
    println!(
        "object {} entries, PostgreSQL {} distinct prefixes, {} sampled",
        report.object_entries, report.pg_entries, report.sampled
    );
    if report.mismatches.is_empty() {
        println!("no differences in the sample");
    } else {
        for line in &report.mismatches {
            println!("mismatch: {line}");
        }
        anyhow::bail!("{} difference(s) found", report.mismatches.len());
    }

    if local != std::path::Path::new(object) {
        let _ = std::fs::remove_file(&local);
    }
    Ok(())
}
