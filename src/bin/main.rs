use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use ipnet::IpNet;
use rayon::prelude::*;
use std::io::{copy, Write};
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

        #[clap(short = 'H', long, default_value = "0.0.0.0")]
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
                                match backup_archive(&rkyv_path, backup_to) {
                                    Ok(()) => {
                                        info!("backup trie written to {}", backup_to);
                                        to_send_heartbeat = true;
                                    }
                                    Err(e) => error!(
                                        "failed to write backup trie to {}: {}",
                                        backup_to, e
                                    ),
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

fn get_tokio_runtime() -> tokio::runtime::Runtime {
    let blocking_cpus = num_threads();
    debug!("using {} cores for parsing html pages", blocking_cpus);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(blocking_cpus)
        .build()
        .unwrap()
}

/// Stream a suffix-compressed source through oneio into an uncompressed local
/// archive. The result is suitable for direct `mmap` access.
fn download_rkyv_transport(url: &str, local_path: &str) -> anyhow::Result<()> {
    let mut reader = oneio::get_reader(url)?;
    let mut writer = std::fs::File::create(local_path)?;
    copy(&mut reader, &mut writer)?;
    writer.sync_all()?;
    Ok(())
}

/// Create a gzip transport copy of a raw rkyv archive. The active archive is
/// intentionally never gzip-compressed because rkyv serving requires mmap.
fn create_rkyv_transport_gzip(source: &str) -> anyhow::Result<std::path::PathBuf> {
    let temp = std::env::temp_dir().join(format!(
        "wayback-rpki-{}-{}.rkyv.gz",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let mut reader = std::fs::File::open(source)?;
    let mut writer = oneio::get_writer(temp.to_str().unwrap())?;
    copy(&mut reader, &mut writer)?;
    writer.flush()?;
    drop(writer);
    Ok(temp)
}

/// Write an archive backup. A `.rkyv.gz` destination is explicitly a
/// transport artifact; the running service continues to mmap the raw `.rkyv`.
fn backup_archive(source: &str, destination: &str) -> anyhow::Result<()> {
    let temporary = if destination.ends_with(".rkyv.gz") {
        Some(create_rkyv_transport_gzip(source)?)
    } else {
        None
    };
    let upload_source = temporary
        .as_ref()
        .and_then(|p| p.to_str())
        .unwrap_or(source);

    let result = match oneio::s3_url_parse(destination) {
        Ok((bucket, key)) => {
            oneio::s3_env_check()?;
            oneio::s3_upload(&bucket, &key, upload_source)?;
            Ok(())
        }
        Err(_) => {
            std::fs::copy(upload_source, destination)?;
            Ok(())
        }
    };
    if let Some(temp) = temporary {
        let _ = std::fs::remove_file(temp);
    }
    result
}

/// Ensure a requested archive is present.
///
/// For a missing `.rkyv`, a local sibling legacy archive is always preferred:
/// `roas_trie.bin.gz`/`.bin` is converted before attempting network bootstrap.
fn check_bootstrap_and_download(path: &str, bootstrap: bool) {
    const LEGACY_BOOTSTRAP_URL: &str = "https://spaces.bgpkit.org/broker/roas_trie.bin.gz";

    if Path::new(path).exists() {
        return;
    }

    if is_rkyv_path(path) {
        if let Some(bin_path) = find_sibling_bin(path) {
            info!(
                "requested {} not found; auto-converting local sibling {} before bootstrap",
                path, bin_path
            );
            auto_convert(&bin_path, path).unwrap();
            return;
        }
    }

    if !bootstrap {
        return;
    }

    if is_rkyv_path(path) {
        info!(
            "no local legacy archive found; attempting compressed rkyv bootstrap {}",
            REMOTE_BOOTSTRAP_URL
        );
        if let Err(rkyv_error) = download_rkyv_transport(REMOTE_BOOTSTRAP_URL, path) {
            // During the transition the v1 archive remains the durable remote
            // bootstrap source. Download it beside the requested rkyv path, then
            // reuse the normal conversion path to create the mmap-ready archive.
            let legacy_path = format!("{}.bin.gz", path.strip_suffix(".rkyv").unwrap_or(path));
            warn!(
                "rkyv bootstrap {} failed: {}; falling back to legacy bootstrap {}",
                REMOTE_BOOTSTRAP_URL, rkyv_error, LEGACY_BOOTSTRAP_URL
            );
            if let Err(legacy_error) = oneio::download(LEGACY_BOOTSTRAP_URL, &legacy_path) {
                error!(
                    "legacy bootstrap {} also failed: {}",
                    LEGACY_BOOTSTRAP_URL, legacy_error
                );
                exit(1);
            }
            if let Err(convert_error) = auto_convert(&legacy_path, path) {
                error!(
                    "failed to convert downloaded legacy bootstrap {}: {}",
                    legacy_path, convert_error
                );
                exit(1);
            }
        }
    } else {
        info!(
            "downloading legacy bootstrap file {} to {}",
            LEGACY_BOOTSTRAP_URL, path
        );
        oneio::download(LEGACY_BOOTSTRAP_URL, path).unwrap();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn unique_path(suffix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "wayback-rpki-main-test-{}-{}{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            suffix
        ))
    }

    #[test]
    fn rkyv_gzip_backup_is_transport_only() {
        let source = unique_path(".rkyv");
        let backup = unique_path(".rkyv.gz");
        let payload = b"raw rkyv archive bytes: must remain mmap-ready locally";
        std::fs::write(&source, payload).unwrap();

        backup_archive(source.to_str().unwrap(), backup.to_str().unwrap()).unwrap();
        assert!(backup.exists());
        assert_ne!(std::fs::read(&backup).unwrap(), payload);

        let mut decoded = Vec::new();
        oneio::get_reader(backup.to_str().unwrap())
            .unwrap()
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, payload);
        assert_eq!(std::fs::read(&source).unwrap(), payload);

        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(backup);
    }

    #[test]
    fn suffix_routing_and_sibling_discovery() {
        assert!(is_rkyv_path("roas_trie.rkyv"));
        assert!(!is_rkyv_path("roas_trie.bin.gz"));

        let rkyv = unique_path(".rkyv");
        let base = rkyv.to_str().unwrap().strip_suffix(".rkyv").unwrap();
        let sibling = format!("{base}.bin.gz");
        std::fs::write(&sibling, b"legacy").unwrap();
        assert_eq!(
            find_sibling_bin(rkyv.to_str().unwrap()),
            Some(sibling.clone())
        );
        let _ = std::fs::remove_file(sibling);
    }
}
