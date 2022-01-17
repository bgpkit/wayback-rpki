use std::sync::mpsc::channel;
use std::thread;
use diesel::dsl::all;
use tracing::{info, Level};
use wayback_rpki::*;
use structopt::StructOpt;
use rayon::prelude::*;
use indicatif::{ProgressBar,ProgressStyle};

#[derive(StructOpt, Debug)]
#[structopt(name="wayback-rpki-updater")]
struct Opts {
    /// NIC
    #[structopt(short, long)]
    nic: String,

    /// Number of parallel chunks
    #[structopt(short, long)]
    chunks: usize,

    /// Latest first
    #[structopt(short, long)]
    latest: bool,
}

fn main() {
    tracing_subscriber::fmt() .with_max_level(Level::INFO) .init();
    let opts: Opts = Opts::from_args();

    // TODO: AFRINIC still has empty date_ranges. Something is buggy there.
    let conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
    let all_files = conn.get_all_files(opts.nic.as_str(), false, opts.latest);
    info!("total of {} roa files to process", all_files.len());
    /*
    all_files.par_chunks(opts.chunks).for_each(|files| {
        let conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
        for file in files {
            let url = file.url.as_str();
            info!("processing {}", url);
            let roas = parse_roas_csv(url);
            if roas.is_empty() {
                info!("file {} is empty, delete from database", url);
                conn.delete_file(url);
            } else {
                conn.insert_roa_entries(&roas);
                conn.mark_file_as_processed(url, true);
            }
        }
    });
    */

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

    let tables = all_files.par_chunks(opts.chunks).map_with(sender_pb.clone(), |s, files| {
        let mut roas_table = RoasTable::new();
        for file in files {
            let url: &str = file.url.as_str();
            // info!("processing {}", url);
            let roas = parse_roas_csv(url);
            roas.iter().for_each(|r|roas_table.insert_entry(r));
            sender_pb.send(url.to_owned()).unwrap();
        }
        roas_table
    }).collect::<Vec<RoasTable>>();

    let merged_table = RoasTable::merge_tables(tables);
    conn.insert_roa_history_entries(&merged_table.export_to_history());
}
