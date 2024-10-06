use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use ipnet::IpNet;
use rayon::prelude::*;
use std::process::exit;
use std::sync::Arc;
use std::thread;
use tabled::settings::Style;
use tabled::Table;
use tokio::runtime::Runtime;
use tokio::sync::RwLock;
use tracing::{debug, error, info, Level};
use wayback_rpki::*;

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
#[clap(name = "wayback-rpki")]
struct Cli {
    /// file path to dump the trie.
    #[clap(default_value = "roas_trie.bin.gz", global = true)]
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

        /// IP prefix to search ROAs for, e.g. `?prefix=
        #[clap(short, long)]
        prefix: Option<IpNet>,

        /// filter by max_len
        #[clap(short, long)]
        max_len: Option<u8>,

        /// limit the date of the ROAs, format: YYYY-MM-DD, e.g. `?date=2022-01-01`
        #[clap(short, long)]
        date: Option<NaiveDate>,

        /// filter results to whether ROA is still current
        #[clap(short, long)]
        current: Option<bool>,
    },
    /// Serve the API
    Serve {
        /// Additional path to backup the trie
        #[clap(long)]
        backup_to: Option<String>,
    },
}

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_ansi(true)
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

    let path = opts.path;

    // check db url
    match opts.subcommands {
        Opts::Rebuild {
            tal,
            chunks_opt,
            from,
            until,
        } => {
            let chunks = chunks_opt.unwrap_or(num_cpus::get());
            let all_files = get_tal_urls(tal)
                .into_iter()
                .flat_map(|tal_url| crawl_tal_after(tal_url.as_str(), from, until))
                .collect::<Vec<RoaFile>>();

            // conn.insert_roa_files(&all_files);
            // let all_files = conn.get_all_files(tal.as_str(), false, latest);
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
                // let mut conn = DbConnection::new();
                let mut writer = oneio::get_writer("wayback-rpki.bootstrap.log").unwrap();
                for (url, _count) in receiver_pb.iter() {
                    // conn.mark_file_as_processed(url.as_str(), true, count);
                    writeln!(writer, "{}", url).unwrap();
                    pb.set_message(url);
                    pb.inc(1);
                }
            });

            // dedicated writer thread
            let handle = thread::spawn(move || {
                let mut trie = RoasTrie::new();
                for entries in receiver_entries.iter() {
                    trie.process_entries(&entries, true);
                }
                trie.compress_dates();
                trie.dump(path.as_str()).unwrap();
            });

            all_files.par_chunks(chunks).for_each_with(
                (sender_pb, sender_entries),
                |(s_pb, s_entries), files| {
                    for file in files {
                        let url: &str = file.url.as_str();
                        // info!("processing {}", url);
                        if let Ok(roas) = parse_roas_csv(url) {
                            let count = roas.len() as i32;
                            s_entries.send(roas).unwrap();
                            s_pb.send((url.to_owned(), count)).unwrap();
                        }
                    }
                },
            );

            handle.join().unwrap();

            info!("bootstrap finished");
        }

        Opts::Update { tal, until } => {
            check_bootstrap_and_download(path.as_str(), opts.bootstrap);
            let mut trie = RoasTrie::load(path.as_str()).unwrap();
            trie.update(tal, until).unwrap();
            trie.dump(path.as_str()).unwrap();
        }

        Opts::Search {
            asn,
            prefix,
            max_len,
            date,
            current,
        } => {
            check_bootstrap_and_download(path.as_str(), opts.bootstrap);
            let trie = RoasTrie::load(path.as_str()).unwrap();
            let results: Vec<RoasLookupEntryTabled> = trie
                .search(prefix, asn, max_len, date, current)
                .into_iter()
                .map(|entry| entry.into())
                .collect();
            println!("{}", Table::new(results).with(Style::markdown()));
        }

        Opts::Fix {} => {
            check_bootstrap_and_download(path.as_str(), opts.bootstrap);
            let mut trie = RoasTrie::load(path.as_str()).unwrap();
            trie.fill_gaps();
            trie.dump(path.as_str()).unwrap();
        }

        Opts::Serve { backup_to } => {
            let mut backup_destinations = vec![backup_to];
            if let Ok(p) = std::env::var("WAYBACK_BACKUP_TO") {
                // replace backup_to with the env variable if it is set
                backup_destinations.push(Some(p));
            }
            for backup_to in &backup_destinations {
                if let Some(backup_to) = backup_to.as_ref() {
                    info!("backup trie will be written to {}", backup_to);
                }
            }

            check_bootstrap_and_download(path.as_str(), opts.bootstrap);
            let trie = RoasTrie::load(path.as_str()).unwrap();
            let trie_lock = Arc::new(RwLock::new(trie));
            let timer_lock = trie_lock.clone();
            let host = "0.0.0.0";

            let update_interval = 60 * 60 * 8;

            thread::spawn(move || {
                let rt = get_tokio_runtime();
                rt.block_on(async {
                    let mut interval =
                        tokio::time::interval(std::time::Duration::from_secs(update_interval));
                    loop {
                        interval.tick().await;

                        info!("creating a backup trie...");
                        // updating from the latest data available
                        let read_lock = timer_lock.read().await;
                        let mut backup = read_lock.clone();
                        drop(read_lock);
                        backup.update(None, None).unwrap();

                        info!("writing updated trie to disk...");
                        match backup.dump(&path) {
                            Ok(_) => {
                                info!("backup trie written to disk: {}", path);
                            }
                            Err(e) => error!("failed to write backup trie to disk: {}", e),
                        }

                        for backup_to in &backup_destinations {
                            if let Some(backup_to) = backup_to.as_ref() {
                                info!("writing additional backup trie to disk at {}...", backup_to);
                                match oneio::s3_url_parse(backup_to) {
                                    Ok((bucket, key)) => {
                                        if oneio::s3_env_check().is_err() {
                                            error!("s3 environment variables not set, skipping backup to s3");
                                        } else {
                                            match oneio::s3_upload(&bucket, &key, path.as_str()) {
                                                Ok(_) => {
                                                    info!("backup trie written to s3: {}", backup_to);
                                                }
                                                Err(_) => {
                                                    error!("failed to write backup trie to s3: {}", backup_to);
                                                }
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        // not a s3 url, copy the current trie to the specified path
                                        // make file system copy of the trie file at path
                                        match std::fs::copy(&path, backup_to) {
                                            Ok(_) => {
                                                info!("backup trie written to disk: {}", backup_to);
                                            }
                                            Err(e) => {
                                                error!("failed to write backup trie to disk: {}", e)
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        info!("replacing backup trie with the original trie...");
                        let mut write_lock = timer_lock.write().await;
                        write_lock.replace(backup);
                        drop(write_lock);

                        info!("wait for {} seconds before next update", update_interval);
                    }
                });
            });

            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(num_cpus::get())
                .enable_all()
                .build()
                .unwrap()
                .block_on(start_api_service(
                    trie_lock,
                    host.to_string(),
                    3000,
                    "/".to_string(),
                ))
                .unwrap();
        }
    }
}

fn get_tokio_runtime() -> Runtime {
    let blocking_cpus = num_cpus::get();

    debug!("using {} cores for parsing html pages", blocking_cpus);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(blocking_cpus)
        .build()
        .unwrap();
    rt
}

/// Check if data file exists, and bootstrap if necessary
fn check_bootstrap_and_download(path: &str, bootstrap: bool) {
    if !std::path::Path::new(path).exists() {
        // if file at `path` does not exist
        if bootstrap {
            // download bootstrap file
            let remote_bootstrap_file = "https://spaces.bgpkit.org/broker/roas_trie.bin.gz";
            info!(
                "downloading bootstrap file {} to {}",
                remote_bootstrap_file, path
            );
            oneio::download(remote_bootstrap_file, path, None).unwrap();
        }
    }
}
