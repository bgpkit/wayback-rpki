use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::collections::HashMap;
use std::thread;
use tracing::{info, Level};
use wayback_rpki::*;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
#[clap(name = "wayback-rpki")]
enum Opts {
    /// Bootstrapping `roa_history` table
    Bootstrap {
        /// limit to specic TAL: afrinic, apnic, arin, lacnic, ripencc
        #[clap(short, long)]
        tal: Option<String>,

        /// Number of parallel chunks
        #[clap(short, long = "chunks")]
        chunks_opt: Option<usize>,

        /// file path to dump the trie
        #[clap(default_value = "roas_trie.bin.gz")]
        path: String,
    },
    /// Find new ROA files and apply changes
    Update {
        /// TAL: afrinic, apnic, arin, lacnic, ripencc; default: all
        #[clap(short, long)]
        tal: Option<String>,
    },
}

fn main() {
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();
    let opts: Opts = Opts::parse();

    // check db url
    let tals_map = HashMap::from([
        ("afrinic", "https://ftp.ripe.net/rpki/afrinic.tal"),
        ("lacnic", "https://ftp.ripe.net/rpki/lacnic.tal"),
        ("apnic", "https://ftp.ripe.net/rpki/apnic.tal"),
        ("ripencc", "https://ftp.ripe.net/rpki/ripencc.tal"),
        ("arin", "https://ftp.ripe.net/rpki/arin.tal"),
    ]);

    match opts {
        Opts::Bootstrap {
            tal,
            chunks_opt,
            path,
        } => {
            let chunks = chunks_opt.unwrap_or(num_cpus::get());
            let tal_urls = match tal {
                None => tals_map.values().map(|url| url.to_string()).collect(),
                Some(tal) => {
                    let url = tals_map
                        .get(tal.as_str())
                        .expect(r#"can only be one of the following "ripencc"|"afrinic"|"apnic"|"arin"|"lacnic""#)
                        .to_string();
                    vec![url]
                }
            };

            let all_files = tal_urls
                .into_iter()
                .flat_map(|tal_url| crawl_tal_after(tal_url.as_str(), None))
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
                for (url, _count) in receiver_pb.iter() {
                    // conn.mark_file_as_processed(url.as_str(), true, count);
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

        Opts::Update { tal } => {
            // The Update subcommand should "catch up" with the latest roas.csv files based on the most recent data files in the database for each tal
            rayon::ThreadPoolBuilder::new()
                .num_threads(50)
                .build_global()
                .unwrap();

            let tal_urls: Vec<(String, String)> = match tal {
                None => tals_map
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                Some(tal) => {
                    let url = tals_map.get(tal.as_str()).expect(r#"can only be one of the following "ripencc"|"afrinic"|"apnic"|"arin"|"lacnic""#).to_string();
                    vec![(tal, url)]
                }
            };

            // let mut conn = DbConnection::new();

            for (tal, _tal_url) in tal_urls {
                info!("start updating roas history for {}", tal.as_str());
                info!(
                    "searching for latest roas.csv.xz files from {}",
                    tal.as_str()
                );

                /*
                // 1. get the latest files date for the given TAL
                let latest_file = conn.get_latest_processed_file(tal.as_str()).unwrap();

                // 2. crawl and find all files *after* the latest date, i.e. the missing files
                let roa_files = crawl_tal_after(tal_url.as_str(), Some(latest_file.file_date));
                conn.insert_roa_files(&roa_files);

                // 3. process the missing files and insert the results into the database
                let all_files = conn.get_all_files(tal.as_str(), true, false);
                info!("start processing {} roas.csv.xz files", all_files.len());
                for file in all_files {
                    info!("start processing {}", file.url.as_str());
                    let roa_entries = parse_roas_csv(file.url.as_str());
                    let count = roa_entries.len();
                    let roa_entries_vec = roa_entries.into_iter().collect::<Vec<RoaEntry>>();
                    info!("total of {} ROA entries to process", roa_entries_vec.len());
                    roa_entries_vec.par_chunks(2000).for_each(|entries| {
                        let mut new_conn = DbConnection::new();
                        new_conn.insert_roa_entries(entries);
                    });
                    conn.mark_file_as_processed(file.url.as_str(), true, count as i32);
                }
                info!("roas history update process finished");
                 */
            }
        }
    }
}
