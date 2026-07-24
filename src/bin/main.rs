use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use ipnet::IpNet;
use rayon::prelude::*;
use std::path::Path;
use std::process::exit;
use std::sync::Arc;
use std::thread;
use tabled::settings::Style;
use tabled::Table;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn, Level};
use wayback_rpki::api::TrieBackend;
use wayback_rpki::*;

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
#[clap(name = "wayback-rpki")]
struct Cli {
    /// file path to the trie archive. Suffix determines the backend:
    /// `.rkyv` → v2 mmap mode, `.bin`/`.bin.gz` → v1 legacy in-memory mode.
    #[clap(default_value = "roas_trie.rkyv", global = true)]
    path: String,

    /// download bootstrap file to help get started quickly
    #[clap(short, long, global = true)]
    bootstrap: bool,

    /// path to an environment variable file
    #[clap(long, global = true)]
    env: Option<String>,

    #[clap(subcommand)]
    subcommands: Opts,
}

#[derive(Subcommand)]
enum Opts {
    /// Rebuild the entire RPKI ROA history data from scratch
    Rebuild {
        /// limit to specific tal: afrinic, apnic, arin, lacnic, ripencc
        #[clap(short, long)]
        tal: Option<String>,

        /// Number of parallel chunks
        #[clap(short, long = "chunks")]
        chunks_opt: Option<usize>,

        /// Date to start from, default no limit
        #[clap(short, long)]
        from: Option<NaiveDate>,

        /// Date to stop at, default no limit
        #[clap(short, long)]
        until: Option<NaiveDate>,
    },
    /// Find new ROA files and apply changes
    Update {
        /// TAL: afrinic, apnic, arin, lacnic, ripencc; default: all
        #[clap(short, long)]
        tal: Option<String>,

        /// Date to stop at, default no limit
        #[clap(short, long)]
        until: Option<NaiveDate>,
    },
    /// Fix potential data issues
    Fix {},
    /// Search for ROAs in history
    Search {
        /// filter results by ASN exact match
        #[clap(short, long)]
        asn: Option<u32>,

        /// IP prefix to search ROAs for
        #[clap(short, long)]
        prefix: Option<IpNet>,

        /// filter by max_len
        #[clap(short, long)]
        max_len: Option<u8>,

        /// limit the date of the ROAs, format: YYYY-MM-DD
        #[clap(short, long)]
        date: Option<NaiveDate>,

        /// filter results to whether ROA is still current
        #[clap(short, long)]
        current: Option<bool>,

        /// only return exact prefix matches (default: true)
        #[clap(short, long)]
        exact: Option<bool>,
    },
    /// Convert a legacy v1 (bincode + ipnet-trie) archive to the v2 rkyv format
    Convert {
        /// path to the legacy v1 archive (e.g. roas_trie.bin.gz)
        #[clap(short, long)]
        from: String,
    },
    /// Serve the API
    Serve {
        /// Additional path to backup the trie
        #[clap(long)]
        backup_to: Option<String>,

        #[clap(short, long, default_value = "0.0.0.0")]
        host: String,

        #[clap(short, long, default_value = "40065")]
        port: u16,

        /// update interval in seconds between data refreshes (default: 8 hours)
        #[clap(short, long, default_value = "28800")]
        update_interval: u64,
    },
}

fn num_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Determine if a path is a v2 rkyv archive or a v1 bincode archive.
fn is_rkyv_path(path: &str) -> bool {
    path.ends_with(".rkyv")
}

/// Find a sibling legacy bin file for a given rkyv path.
/// e.g. `roas_trie.rkyv` → check `roas_trie.bin.gz`, then `roas_trie.bin`
fn find_sibling_bin(rkyv_path: &str) -> Option<String> {
    let base = rkyv_path.strip_suffix(".rkyv").unwrap_or(rkyv_path);
    for suffix in &[".bin.gz", ".bin"] {
        let candidate = format!("{}{}", base, suffix);
        if Path::new(&candidate).exists() {
            return Some(candidate);
        }
    }
    None
}

/// Auto-convert a legacy v1 `.bin[.gz]` archive to v2 rkyv at the given path.
fn auto_convert(bin_path: &str, rkyv_path: &str) -> anyhow::Result<()> {
    info!(
        "auto-converting legacy archive {} → {} ...",
        bin_path, rkyv_path
    );
    let legacy = wayback_rpki::legacy::LegacyRoasTrie::load(bin_path)?;
    let mut builder = legacy.to_builder();
    builder.dump(rkyv_path)?;
    info!("auto-conversion complete: {}", rkyv_path);
    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_ansi(false)
        .init();

    let opts = Cli::parse();
    match opts.env {
        Some(env_path) => {
            match dotenvy::from_path(env_path.as_str()) {
                Ok(_) => {
                    info!("loaded environment variables from {}", env_path);
                }
                Err(_) => {
                    error!("failed to load environment variables from {}", env_path);
                    exit(1);
                }
            };
        }
        None => {
            dotenvy::dotenv().ok();
            info!("no environment variable file specified, load from .env or alike");
        }
    }

    let path = opts.path.clone();

    match opts.subcommands {
        Opts::Rebuild {
            tal,
            chunks_opt,
            from,
            until,
        } => {
            let chunks = chunks_opt.unwrap_or_else(num_threads);
            let all_files = get_tal_urls(tal)
                .into_iter()
                .flat_map(|tal_url| crawl_tal_after(tal_url.as_str(), from, until))
                .collect::<Vec<RoaFile>>();

            info!("total of {} roa files to process", all_files.len());

            let (sender_pb, receiver_pb) = std::sync::mpsc::sync_channel::<(String, i32)>(20);
            let (sender_entries, receiver_entries) =
                std::sync::mpsc::sync_channel::<Vec<RoaEntry>>(2000);

            let total_files = all_files.len();

            let pb = ProgressBar::new(total_files as u64);
            let sty = ProgressStyle::default_bar()
                .template(
                    "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} [{eta_precise}] {msg}",
                )
                .unwrap()
                .progress_chars("##-");
            pb.set_style(sty);

            // dedicated thread for showing progress of the parsing
            thread::spawn(move || {
                let mut writer = oneio::get_writer("wayback-rpki.bootstrap.log").unwrap();
                for (url, _count) in receiver_pb.iter() {
                    writeln!(writer, "{}", url).unwrap();
                    pb.set_message(url);
                    pb.inc(1);
                }
            });

            // dedicated writer thread
            let path2 = path.clone();
            let handle = thread::spawn(move || {
                let mut trie = RoasTrieMut::new();
                for entries in receiver_entries.iter() {
                    trie.process_entries(&entries, true);
                }
                trie.dump(path2.as_str()).unwrap();
                info!(
                    "bootstrap finished: {} prefixes written to {}",
                    trie.len(),
                    path2
                );
            });

            all_files.par_chunks(chunks).for_each_with(
                (sender_pb, sender_entries),
                |(s_pb, s_entries), files| {
                    for file in files {
                        let url: &str = file.url.as_str();
                        match parse_roas_csv(url) {
                            Ok(roas) => {
                                let count = roas.len() as i32;
                                s_entries.send(roas).unwrap();
                                s_pb.send((url.to_owned(), count)).unwrap();
                            }
                            Err(e) => {
                                warn!("failed to parse {}: {}", url, e);
                                s_pb.send((url.to_owned(), 0)).unwrap();
                            }
                        }
                    }
                },
            );

            handle.join().unwrap();
        }

        Opts::Update { tal, until } => {
            check_bootstrap_and_download(&path, opts.bootstrap);
            ensure_data_available(&path);

            if is_rkyv_path(&path) {
                let mut trie = RoasTrieMut::load(&path).unwrap();
                trie.update(tal, until).unwrap();
                trie.dump(&path).unwrap();
            } else {
                let mut trie = wayback_rpki::legacy::LegacyRoasTrie::load(&path).unwrap();
                trie.update(tal, until).unwrap();
                trie.dump(&path).unwrap();
            }
        }

        Opts::Search {
            asn,
            prefix,
            max_len,
            date,
            current,
            exact,
        } => {
            check_bootstrap_and_download(&path, opts.bootstrap);
            ensure_data_available(&path);

            let results: Vec<RoasLookupEntryTabled> = if is_rkyv_path(&path) {
                let trie = RoasTrie::open(&path).unwrap();
                trie.search(prefix, asn, max_len, date, current, exact.unwrap_or(true))
                    .into_iter()
                    .map(|e| e.into())
                    .collect()
            } else {
                let trie = wayback_rpki::legacy::LegacyRoasTrie::load(&path).unwrap();
                trie.search(prefix, asn, max_len, date, current, exact.unwrap_or(true))
                    .into_iter()
                    .map(|e| e.into())
                    .collect()
            };
            println!("{}", Table::new(results).with(Style::markdown()));
        }

        Opts::Fix {} => {
            check_bootstrap_and_download(&path, opts.bootstrap);
            ensure_data_available(&path);

            if is_rkyv_path(&path) {
                let mut trie = RoasTrieMut::load(&path).unwrap();
                trie.fill_gaps();
                trie.dump(&path).unwrap();
            } else {
                let mut trie = wayback_rpki::legacy::LegacyRoasTrie::load(&path).unwrap();
                trie.fill_gaps();
                trie.dump(&path).unwrap();
            }
        }

        Opts::Convert { from } => {
            let legacy = wayback_rpki::legacy::LegacyRoasTrie::load(&from).unwrap();
            let mut builder = legacy.to_builder();
            builder.dump(&path).unwrap();
            info!("converted legacy archive {} -> {}", from, path);
        }

        Opts::Serve {
            backup_to,
            host,
            port,
            update_interval,
        } => {
            let mut backup_destinations = vec![backup_to];
            if let Ok(p) = std::env::var("WAYBACK_BACKUP_TO") {
                backup_destinations.push(Some(p));
            }
            for backup_to in &backup_destinations {
                if let Some(backup_to) = backup_to.as_ref() {
                    info!("backup trie will be written to {}", backup_to);
                }
            }
            let backup_heartbeat_url = std::env::var("WAYBACK_BACKUP_HEARTBEAT_URL").ok();
            if let Some(backup_heartbeat_url) = backup_heartbeat_url.as_ref() {
                info!("backup heartbeat will be sent to {}", backup_heartbeat_url);
            }

            check_bootstrap_and_download(&path, opts.bootstrap);
            ensure_data_available(&path);

            let backend = if is_rkyv_path(&path) {
                info!("opening v2 rkyv archive (mmap mode): {}", path);
                TrieBackend::V2(RoasTrie::open(&path).unwrap())
            } else {
                info!("opening v1 legacy archive (in-memory mode): {}", path);
                TrieBackend::V1(wayback_rpki::legacy::LegacyRoasTrie::load(&path).unwrap())
            };
            let trie_lock: Arc<RwLock<TrieBackend>> = Arc::new(RwLock::new(backend));
            let timer_lock = trie_lock.clone();
            let serve_path = path.clone();

            thread::spawn(move || {
                let rt = get_tokio_runtime();
                rt.block_on(async {
                    let mut interval =
                        tokio::time::interval(std::time::Duration::from_secs(update_interval));
                    loop {
                        interval.tick().await;

                        info!("updating trie from latest data...");
                        let rkyv_path = serve_path.clone();
                        let is_rkyv = is_rkyv_path(&rkyv_path);

                        let update_result = if is_rkyv {
                            RoasTrieMut::load(&rkyv_path)
                                .map_err(|e| e.to_string())
                                .and_then(|mut t| {
                                    t.update(None, None).map_err(|e| e.to_string())?;
                                    t.dump(&rkyv_path).map_err(|e| e.to_string())?;
                                    Ok(())
                                })
                        } else {
                            wayback_rpki::legacy::LegacyRoasTrie::load(&rkyv_path)
                                .map_err(|e| e.to_string())
                                .and_then(|mut t| {
                                    t.update(None, None).map_err(|e| e.to_string())?;
                                    t.dump(&rkyv_path).map_err(|e| e.to_string())?;
                                    Ok(())
                                })
                        };

                        if let Err(e) = update_result {
                            error!("trie update failed: {}", e);
                            continue;
                        }
                        info!("updated trie written to disk: {}", rkyv_path);

                        let mut to_send_heartbeat = false;
                        for backup_to in &backup_destinations {
                            if let Some(backup_to) = backup_to.as_ref() {
                                info!("writing additional backup trie to {}...", backup_to);
                                match oneio::s3_url_parse(backup_to) {
                                    Ok((bucket, key)) => {
                                        if oneio::s3_env_check().is_err() {
                                            error!(
                                                "s3 environment variables not set, skipping backup to s3"
                                            );
                                        } else {
                                            match oneio::s3_upload(&bucket, &key, rkyv_path.as_str()) {
                                                Ok(_) => {
                                                    info!("backup trie written to s3: {}", backup_to);
                                                    to_send_heartbeat = true;
                                                }
                                                Err(e) => {
                                                    error!(
                                                        "failed to write backup trie to s3 {}: {}",
                                                        backup_to, e
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    Err(_) => match std::fs::copy(&rkyv_path, backup_to) {
                                        Ok(_) => {
                                            info!("backup trie written to disk: {}", backup_to);
                                            to_send_heartbeat = true;
                                        }
                                        Err(e) => {
                                            error!(
                                                "failed to write backup trie to disk {}: {}",
                                                backup_to, e
                                            )
                                        }
                                    },
                                }
                            }
                        }

                        if to_send_heartbeat {
                            if let Some(backup_heartbeat_url) = backup_heartbeat_url.as_ref() {
                                info!("sending heartbeat to {}", backup_heartbeat_url);
                                if let Err(e) = oneio::read_to_string_lossy(backup_heartbeat_url) {
                                    error!("failed to send heartbeat: {}", e);
                                }
                            }
                        }

                        // Swap in the updated archive: re-open and replace.
                        info!("swapping serving trie to updated archive...");
                        let new_backend = if is_rkyv {
                            match RoasTrie::open(&rkyv_path) {
                                Ok(t) => Some(TrieBackend::V2(t)),
                                Err(e) => {
                                    error!("failed to re-open updated trie: {}", e);
                                    None
                                }
                            }
                        } else {
                            match wayback_rpki::legacy::LegacyRoasTrie::load(&rkyv_path) {
                                Ok(t) => Some(TrieBackend::V1(t)),
                                Err(e) => {
                                    error!("failed to re-load legacy trie: {}", e);
                                    None
                                }
                            }
                        };
                        if let Some(b) = new_backend {
                            let mut write_lock = timer_lock.write().await;
                            *write_lock = b;
                            drop(write_lock);
                            info!("serving trie swapped");
                        }

                        info!("wait for {} seconds before next update", update_interval);
                    }
                });
            });

            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(num_threads())
                .enable_all()
                .build()
                .unwrap()
                .block_on(start_api_service(trie_lock, host, port, "/".to_string()))
                .unwrap();
        }
    }
}

use std::io::Write;

fn get_tokio_runtime() -> tokio::runtime::Runtime {
    let blocking_cpus = num_threads();
    debug!("using {} cores for parsing html pages", blocking_cpus);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(blocking_cpus)
        .build()
        .unwrap()
}

/// Check if data file exists, and bootstrap if necessary
fn check_bootstrap_and_download(path: &str, bootstrap: bool) {
    if !Path::new(path).exists() && bootstrap {
        info!(
            "downloading bootstrap file {} to {}",
            REMOTE_BOOTSTRAP_URL, path
        );
        oneio::download(REMOTE_BOOTSTRAP_URL, path).unwrap();
    }
}

/// Ensure the data file exists. For `.rkyv` paths, if the file is missing,
/// attempt to find and auto-convert a sibling `.bin[.gz]` file.
fn ensure_data_available(path: &str) {
    if Path::new(path).exists() {
        return;
    }

    // For rkyv paths, try to find a sibling legacy bin file and auto-convert
    if is_rkyv_path(path) {
        if let Some(bin_path) = find_sibling_bin(path) {
            info!(
                "requested {} not found; auto-converting from sibling {}",
                path, bin_path
            );
            match auto_convert(&bin_path, path) {
                Ok(()) => return,
                Err(e) => {
                    error!("auto-conversion from {} failed: {}", bin_path, e);
                }
            }
        }
    }

    // Still missing — the caller's check_bootstrap_and_download should have
    // already handled the download case. If we reach here, there's no data.
    error!(
        "data file {} does not exist and no sibling bin file found",
        path
    );
    exit(1);
}
