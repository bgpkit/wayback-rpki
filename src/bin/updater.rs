use std::thread;
use tracing::{info, Level};
use wayback_rpki::*;
use structopt::StructOpt;
use rayon::prelude::*;
use indicatif::{ProgressBar,ProgressStyle};

#[derive(StructOpt, Debug)]
#[structopt(name="wayback-rpki")]
enum Opts {
    // TODO: bootstrap still requires the `roa_files` table be populated beforehand.
    // bootstrapping `roa_history` table
    Bootstrap {
        /// NIC
        #[structopt(short, long)]
        nic: String,

        /// Number of parallel chunks
        #[structopt(short, long)]
        chunks: usize,

        /// Latest first
        #[structopt(short, long)]
        latest: bool,
    },
    // find new ROA files and apply changes
    Update {
        /// NIC
        #[structopt(short, long)]
        nic: String,
    }
}

fn main() {
    tracing_subscriber::fmt() .with_max_level(Level::INFO) .init();
    let opts: Opts = Opts::from_args();

    match opts{

        Opts::Bootstrap { nic, chunks, latest } => {

            let conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
            let all_files = conn.get_all_files(nic.as_str(), false, latest);
            info!("total of {} roa files to process", all_files.len());

            let (sender_pb, receiver_pb) = std::sync::mpsc::sync_channel::<String>(20);

            let total_files = all_files.len();
            // dedicated thread for showing progress of the parsing
            thread::spawn(move || {
                let sty = ProgressStyle::default_bar()
                    .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} [{eta_precise}] {msg}")
                    .progress_chars("##-");
                let pb = ProgressBar::new(total_files as u64);
                pb.set_style(sty);
                for msg in receiver_pb.iter() {
                    pb.set_message(msg);
                    pb.inc(1);
                }
            });

            let tables = all_files.par_chunks(chunks).map_with(sender_pb, |s, files| {
                let mut roas_table = RoasTable::new();
                for file in files {
                    let url: &str = file.url.as_str();
                    // info!("processing {}", url);
                    let roas = parse_roas_csv(url);
                    roas.iter().for_each(|r|roas_table.insert_entry(r));
                    s.send(url.to_owned()).unwrap();
                }
                roas_table
            }).collect::<Vec<RoasTable>>();

            let merged_table = RoasTable::merge_tables(tables);
            conn.insert_roa_history_entries(&merged_table.export_to_history());

            info!("bootstrap finished");
        }
        Opts::Update { nic } => {
            info!("start updating roas history for {}", nic.as_str());
            let nic_url = match nic.as_str() {
                "ripencc"|"afrinic"|"apnic"|"arin"|"lacnic" => {
                    format!("https://ftp.ripe.net/rpki/{}.tal", nic.as_str())
                }
                _ => {
                    panic!(r#"can only be one of the following "ripencc"|"afrinic"|"apnic"|"arin"|"lacnic""#);
                }
            };

            info!("searching for latest roas.csv files from {}", nic.as_str());
            let roa_files = crawl_nic(nic_url.as_str(), false);
            let conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
            conn.insert_roa_files(&roa_files);

            let all_files = conn.get_all_files(nic.as_str(), true, false);
            info!("start processing {} roas.csv files", all_files.len());
            for file in all_files {
                info!("start processing {}", file.url.as_str());
                let roa_entries = parse_roas_csv(file.url.as_str());
                let roa_entries_vec = roa_entries.into_iter().collect::<Vec<RoaEntry>>();
                info!("total of {} ROA entries to process", roa_entries_vec.len());
                roa_entries_vec.par_chunks(2000).for_each(|entries|{
                    let new_conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
                    new_conn.insert_roa_entries(entries);
                });
                conn.mark_file_as_processed(file.url.as_str(), true);
            }
            info!("roas history update process finished");
        }
    }
}
