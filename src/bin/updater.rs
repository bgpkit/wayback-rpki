use tracing::{info, Level};
use wayback_rpki::*;
use structopt::StructOpt;
use rayon::prelude::*;

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
    let all_files = conn.get_all_files(opts.nic.as_str(), true, opts.latest);
    info!("total of {} roa files to process", all_files.len());
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
}
