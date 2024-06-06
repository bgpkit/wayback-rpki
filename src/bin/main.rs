use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use ipnet::IpNet;
use rayon::prelude::*;
use std::collections::HashMap;
use std::thread;
use tabled::settings::Style;
use tabled::Table;
use tracing::{info, Level};
use wayback_rpki::*;

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
#[clap(name = "wayback-rpki")]
struct Cli {
    /// file path to dump the trie
    #[clap(default_value = "roas_trie.bin.gz", global = true)]
    path: String,

    #[clap(subcommand)]
    subcommands: Opts,
}

#[derive(Subcommand)]
enum Opts {
    /// Bootstrapping `roa_history` table
    Bootstrap {
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
    Serve {},
}
fn get_tal_urls(tal: Option<String>) -> Vec<String> {
    let tal_map = HashMap::from([
        ("afrinic", "https://ftp.ripe.net/rpki/afrinic.tal"),
        ("lacnic", "https://ftp.ripe.net/rpki/lacnic.tal"),
        ("apnic", "https://ftp.ripe.net/rpki/apnic.tal"),
        ("ripencc", "https://ftp.ripe.net/rpki/ripencc.tal"),
        ("arin", "https://ftp.ripe.net/rpki/arin.tal"),
    ]);

    match tal {
        None => tal_map.values().map(|url| url.to_string()).collect(),
        Some(tal) => {
            let url = tal_map
                .get(tal.as_str())
                .expect(r#"can only be one of the following "ripencc"|"afrinic"|"apnic"|"arin"|"lacnic""#)
                .to_string();
            vec![url]
        }
    }
}

fn main() {
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();
    let opts = Cli::parse();
    let path = opts.path;

    // check db url
    match opts.subcommands {
        Opts::Bootstrap {
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
            let mut trie = RoasTrie::load(path.as_str()).unwrap();
            let mut all_files = get_tal_urls(tal)
                .into_iter()
                .flat_map(|tal_url| {
                    crawl_tal_after(
                        tal_url.as_str(),
                        Some(trie.get_latest_date() + chrono::Duration::days(1)),
                        until,
                    )
                })
                .collect::<Vec<RoaFile>>();

            if all_files.is_empty() {
                info!("ROAS trie is up to date. No new files found.");
                return;
            }

            // sort by date
            all_files.sort_by(|a, b| a.file_date.cmp(&b.file_date));

            for file in all_files {
                info!("processing {}", file.url.as_str());
                let url: &str = file.url.as_str();
                if let Ok(roas) = parse_roas_csv(url) {
                    trie.process_entries(&roas, false);
                }
            }

            trie.dump(path.as_str()).unwrap();
        }
        Opts::Search {
            asn,
            prefix,
            max_len,
            date,
            current,
        } => {
            let trie = RoasTrie::load(path.as_str()).unwrap();
            let results: Vec<RoasLookupEntryTabled> = trie
                .search(prefix, asn, max_len, date, current)
                .into_iter()
                .map(|entry| entry.into())
                .collect();
            println!("{}", Table::new(results).with(Style::markdown()));
        }
        Opts::Fix {} => {
            let mut trie = RoasTrie::load(path.as_str()).unwrap();
            trie.fill_gaps();
            trie.dump(path.as_str()).unwrap();
        }

        Opts::Serve {} => {
            let trie = RoasTrie::load(path.as_str()).unwrap();
            let host = "0.0.0.0";
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(num_cpus::get())
                .enable_all()
                .build()
                .unwrap()
                .block_on(start_api_service(
                    trie,
                    host.to_string(),
                    3000,
                    "/".to_string(),
                ))
                .unwrap();
        }
    }
}
