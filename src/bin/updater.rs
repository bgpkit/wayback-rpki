use std::{env, thread};
use tracing::{info, Level};
use wayback_rpki::*;
use structopt::StructOpt;
use rayon::prelude::*;
use indicatif::{ProgressBar,ProgressStyle};

#[derive(StructOpt, Debug)]
#[structopt(name="wayback-rpki")]
enum Opts {
    // bootstrapping `roa_history` table
    Bootstrap {
        /// NIC
        #[structopt(short, long)]
        tal: String,

        /// Number of parallel chunks
        #[structopt(short, long)]
        chunks: usize,
    },
    // find new ROA files and apply changes
    Update {
        /// NIC
        #[structopt(short, long)]
        tal: String,
    }
}

fn main() {
    tracing_subscriber::fmt() .with_max_level(Level::INFO) .init();
    let opts: Opts = Opts::from_args();

    // check db url
    let _db_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    match opts{

        Opts::Bootstrap { tal, chunks} => {
            let conn = DbConnection::new();

            let tal_url = match tal.as_str() {
                "afrinic"|"lacnic"| "apnic"| "ripencc"| "arin" => {
                    format!("https://ftp.ripe.net/rpki/{}.tal",tal.as_str())
                }
                _ => {
                    panic!("unknown tal: {}", tal);
                }
            };

            let all_files = crawl_tal(tal_url.as_str(), true);
            conn.insert_roa_files_2(&all_files);
            // let all_files = conn.get_all_files(tal.as_str(), false, latest);
            info!("total of {} roa files to process", all_files.len());

            let (sender_pb, receiver_pb) = std::sync::mpsc::sync_channel::<(String, i32)>(20);

            let total_files = all_files.len();
            // dedicated thread for showing progress of the parsing
            thread::spawn(move || {
                let conn = DbConnection::new();
                let sty = ProgressStyle::default_bar()
                    .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} [{eta_precise}] {msg}")
                    .progress_chars("##-");
                let pb = ProgressBar::new(total_files as u64);
                pb.set_style(sty);
                for (url, count) in receiver_pb.iter() {
                    conn.mark_file_as_processed(url.as_str(), true, count);
                    pb.set_message(url);
                    pb.inc(1);
                }
            });

            let tables = all_files.par_chunks(chunks).map_with(sender_pb, |s, files| {
                let mut roas_table = RoasTable::new();
                for file in files {
                    let url: &str = file.url.as_str();
                    // info!("processing {}", url);
                    let roas = parse_roas_csv(url);
                    let count = roas.len() as i32;
                    roas.iter().for_each(|r|roas_table.insert_entry(r));
                    s.send((url.to_owned(), count)).unwrap();
                }
                roas_table
            }).collect::<Vec<RoasTable>>();

            let merged_table = RoasTable::merge_tables(tables);
            conn.insert_roa_history_2_entries(&merged_table.export_to_history());

            info!("bootstrap finished");
        }
        Opts::Update { tal } => {
            info!("start updating roas history for {}", tal.as_str());
            let tal_url = match tal.as_str() {
                "ripencc"|"afrinic"|"apnic"|"arin"|"lacnic" => {
                    format!("https://ftp.ripe.net/rpki/{}.tal", tal.as_str())
                }
                _ => {
                    panic!(r#"can only be one of the following "ripencc"|"afrinic"|"apnic"|"arin"|"lacnic""#);
                }
            };

            info!("searching for latest roas.csv files from {}", tal.as_str());
            let roa_files = crawl_tal(tal_url.as_str(), false);
            let conn = DbConnection::new();
            conn.insert_roa_files_2(&roa_files);

            let all_files = conn.get_all_files(tal.as_str(), true, false);
            info!("start processing {} roas.csv files", all_files.len());
            for file in all_files {
                info!("start processing {}", file.url.as_str());
                let roa_entries = parse_roas_csv(file.url.as_str());
                let count = roa_entries.len();
                let roa_entries_vec = roa_entries.into_iter().collect::<Vec<RoaEntry>>();
                info!("total of {} ROA entries to process", roa_entries_vec.len());
                roa_entries_vec.par_chunks(2000).for_each(|entries|{
                    let new_conn = DbConnection::new();
                    new_conn.insert_roa_entries(entries);
                });
                conn.mark_file_as_processed(file.url.as_str(), true, count as i32);
            }
            info!("roas history update process finished");
        }
    }
}
