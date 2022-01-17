use tracing::{info, Level};
use wayback_rpki::*;
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
#[structopt(name="wayback-rpki-updater")]
struct Opts {
    /// Only latest data
    #[structopt(short, long)]
    latest: bool,

    /// NIC
    #[structopt(short, long)]
    nic: String,
}

fn main() {
    tracing_subscriber::fmt() .with_max_level(Level::INFO) .init();
    let opts: Opts = Opts::from_args();

    let nic_url = match opts.nic.as_str() {
        "ripencc"|"afrinic"|"apnic"|"arin"|"lacnic" => {
            format!("https://ftp.ripe.net/rpki/{}.tal", opts.nic.as_str())
        }
        _ => {
            panic!(r#"can only be one of the following "ripencc"|"afrinic"|"apnic"|"arin"|"lacnic""#);
        }
    };

    if opts.latest {
        info!("getting only the latest data");
        let roa_files = crawl_nic(nic_url.as_str(), false);
        let conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
        conn.insert_roa_files(&roa_files);
    } else {
        info!("getting all data");
        let roa_files = crawl_nic(nic_url.as_str(), true);
        let conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
        conn.insert_roa_files(&roa_files);
    }
}
