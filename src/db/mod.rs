use std::collections::{Bound, HashSet};
use std::str::FromStr;
use chrono::{Duration, NaiveDate};
use diesel::prelude::*;
use diesel::table;
use diesel::pg::PgConnection;
use ipnetwork::IpNetwork;
use crate::RoaEntry;

table! {
    roa_files (nic, file_date) {
        url -> Text,
        nic -> Text,
        file_date -> Date,
        processed -> Bool,
    }
}

#[derive(Debug, Queryable, Insertable)]
#[table_name="roa_files"]
pub struct RoaFile {
    pub nic: String,
    pub url: String,
    pub file_date: chrono::NaiveDate,
    pub processed: bool,
}

table! {
    roa_history (prefix, asn) {
        nic -> Text,
        prefix -> Cidr,
        asn -> BigInt,
        date_ranges -> Array<Range<Date>>,
    }
}

#[derive(Debug, Queryable, Insertable)]
#[table_name="roa_history"]
pub struct RoaHistoryEntry {
    pub nic: String,
    pub prefix: IpNetwork,
    pub asn: i64,
    pub date_ranges: Vec<(Bound<NaiveDate>, Bound<NaiveDate>)>
}

pub struct DbConnection {
    conn: PgConnection,
}

#[inline]
fn bound_to_date(v: Bound<NaiveDate>, delta: Duration) -> NaiveDate{
    match v {
        Bound::Included(d) => d,
        Bound::Excluded(d) => d + delta,
        _ => panic!("Date cannot be unbounded")
    }
}

impl DbConnection {
    pub fn new(db_url: &str) -> DbConnection {
        let conn = PgConnection::establish(db_url).unwrap();
        DbConnection { conn }
    }

    pub fn insert_roa_files(&self, files: &Vec<RoaFile>) {
        use self::roa_files::dsl::*;
        diesel::insert_into(roa_files).values(files).on_conflict_do_nothing().execute(&self.conn).unwrap();
    }

    pub fn insert_roa_entries(&self, entries: &HashSet<RoaEntry>) {
        use crate::roa_history::dsl::*;

        for entry in entries {
            let e = self.get_history_entry(entry.prefix.as_str(), entry.asn as i64);
            let entry_prefix = IpNetwork::from_str(entry.prefix.as_str()).unwrap();
            match e {
                None => {
                    // we have not seen this prefix before
                    let entry = RoaHistoryEntry{
                        nic: entry.nic.clone(),
                        prefix: entry_prefix,
                        asn: entry.asn as i64,
                        date_ranges: vec![(Bound::Included(entry.date), Bound::Included(entry.date))]
                    };
                    diesel::insert_into(roa_history).values(entry).on_conflict_do_nothing().execute(&self.conn).unwrap();
                }
                Some(history) => {
                    let mut new_ranges: Vec<(Bound<NaiveDate>, Bound<NaiveDate>)> = vec![];
                    let mut found = false;
                    let mut skip_update = false;
                    for (begin, end) in history.date_ranges {
                        if !found {
                            let mut end_date = bound_to_date(end, Duration::days(-1));
                            let mut begin_date = bound_to_date(begin, Duration::days(1));

                            if entry.date == end_date + Duration::days(1) {
                                end_date = end_date + Duration::days(1);
                                found = true;
                            } else if entry.date == begin_date - Duration::days(1) {
                                begin_date = begin_date - Duration::days(1);
                                found = true;
                            } else if entry.date>=begin_date && entry.date <= end_date {
                                // in between a existing range, skip
                                found = true;
                                // no need to do any db operation
                                skip_update = true;
                            }
                            new_ranges.push((Bound::Included(begin_date), Bound::Included(end_date)));
                        } else {
                            new_ranges.push((begin, end))
                        }
                    }

                    if !found {
                        // non of the existing range can cover the entry, create a new one
                        new_ranges.push((Bound::Included(entry.date), Bound::Included(entry.date)));
                        new_ranges.sort_by(|a,b| {
                            let d_a = bound_to_date(a.0, Duration::days(0));
                            let d_b = bound_to_date(b.0, Duration::days(0));
                            d_a.partial_cmp(&d_b).unwrap()
                        });
                    }


                    if !skip_update{
                        let mut merged_ranges = vec![];
                        let mut i = 0;
                        while i < new_ranges.len()-1 {
                            let a_begin = bound_to_date(new_ranges[i].0, Duration::days(1));
                            let mut a_end = bound_to_date(new_ranges[i].1, Duration::days(-1));
                            let b_begin = bound_to_date(new_ranges[i+1].0, Duration::days(1));
                            if a_end == b_begin - Duration::days(1) {
                                a_end = bound_to_date(new_ranges[i+1].1, Duration::days(-1));
                                merged_ranges.push((Bound::Included(a_begin), Bound::Included(a_end)));
                                i += 1;
                            } else {
                                merged_ranges.push(new_ranges[i])
                            }

                            i += 1;
                        }

                        diesel::update(roa_history.filter(prefix.eq(&entry_prefix)))
                            .set(date_ranges.eq(merged_ranges))
                            .execute(&self.conn).unwrap();
                    }
                }
            }
        }
    }

    pub fn get_history_entry(&self, prefix_str: &str, as_number: i64) -> Option<RoaHistoryEntry> {
        use crate::roa_history::dsl::*;

        match roa_history.find((&IpNetwork::from_str(prefix_str).unwrap(), as_number)).first::<RoaHistoryEntry>(&self.conn) {
            Ok(entry) => Some(entry),
            Err(_) => None
        }
    }

    pub fn get_all(&self) -> Vec<RoaHistoryEntry> {
        use crate::roa_history::dsl::*;
        let res = roa_history.load::<RoaHistoryEntry>(&self.conn).unwrap();
        res
    }
}

#[cfg(test)]
mod tests {
    use tracing::{info, Level};
    use crate::{crawl_nic, parse_roas_csv};
    use super::*;

    #[test]
    fn test_connection() {
        let _conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
    }

    #[test]
    fn test_insert_files() {
        tracing_subscriber::fmt() .with_max_level(Level::INFO) .init();
        let roa_files = crawl_nic("https://ftp.ripe.net/rpki/ripencc.tal", false);

        let conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
        conn.insert_roa_files(&roa_files);
    }

    #[test]
    fn test_get_all_entry() {
        let conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
        let entries = conn.get_all();
        dbg!(&entries);
    }

    #[test]
    fn test_get_single_entry() {
        let conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
        let entry = conn.get_history_entry("192.168.0.0/24", 1234);
        dbg!(&entry);
        let entry = conn.get_history_entry("192.168.0.0/2", 1234);
        dbg!(&entry);
    }

    #[test]
    fn test_insert() {
        tracing_subscriber::fmt() .with_max_level(Level::INFO) .init();
        info!("start");
        let roas = parse_roas_csv("https://ftp.ripe.net/rpki/afrinic.tal/2022/01/13/roas.csv");
        let conn = DbConnection::new("postgres://bgpkit_admin:bgpkit@10.2.2.103/bgpkit_rpki");
        conn.insert_roa_entries(&roas);
        info!("end");
    }
}