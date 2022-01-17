use tracing::{info, Level};
use wayback_rpki::*;
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
#[structopt(name="wayback-rpki-updater")]
struct Opts {
    /// NIC
    #[structopt(short, long)]
    nic: String,

    /// Latest first
    #[structopt(short, long)]
    latest: bool,
}

fn main() {
    tracing_subscriber::fmt() .with_max_level(Level::INFO) .init();
    let opts: Opts = Opts::from_args();

    let conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
    let all_files = conn.get_all_files(opts.nic.as_str(), true, opts.latest);
    info!("total of {} roa files to process", all_files.len());
    let conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
    all_files.iter().for_each(|file|{
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
    });
}
