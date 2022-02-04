use std::collections::Bound;
use std::env;
use chrono::{Duration, NaiveDate};
use diesel::prelude::*;
use diesel::table;
use diesel::pg::PgConnection;
use ipnetwork::IpNetwork;
use crate::RoaEntry;

table! {
    roa_files_2 (tal, file_date) {
        url -> Text,
        tal -> Text,
        file_date -> Date,
        rows_count -> Integer,
        processed -> Bool,
    }
}

#[derive(Debug, Queryable, Insertable)]
#[table_name="roa_files_2"]
pub struct RoaFile {
    pub url: String,
    pub tal: String,
    pub file_date: chrono::NaiveDate,
    pub rows_count: i32,
    pub processed: bool,
}

table! {
    roa_history_2 (prefix, asn, max_len) {
        tal -> Text,
        prefix -> Cidr,
        asn -> BigInt,
        date_ranges -> Array<Range<Date>>,
        max_len -> Integer,
    }
}

#[derive(Debug, Queryable, Insertable)]
#[table_name="roa_history_2"]
pub struct RoaHistoryEntry {
    pub tal: String,
    pub prefix: IpNetwork,
    pub asn: i64,
    pub date_ranges: Vec<(Bound<NaiveDate>, Bound<NaiveDate>)>,
    pub max_len: i32,
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
    pub fn new() -> DbConnection {
        dotenv::dotenv().ok();
        let db_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let conn = PgConnection::establish(db_url.as_str()).unwrap();
        DbConnection { conn }
    }

    pub fn insert_roa_files_2(&self, files: &Vec<RoaFile>) {
        use self::roa_files_2::dsl::*;
        diesel::insert_into(roa_files_2).values(files).on_conflict_do_nothing().execute(&self.conn).unwrap();
    }

    pub fn insert_roa_history_2_entries(&self, entries: &Vec<RoaHistoryEntry>) {
        use crate::roa_history_2::dsl::*;
        entries.chunks(5000).for_each(|chunk|{
            diesel::insert_into(roa_history_2).values(chunk).on_conflict_do_nothing().execute(&self.conn).unwrap();
        });
    }

    pub fn insert_roa_entries<'a>(&self, entries: impl IntoIterator<Item=&'a RoaEntry>) {
        use crate::roa_history_2::dsl::*;

        for entry in entries {
            let e = self.get_history_entry(&entry.prefix, entry.max_len, entry.asn as i64);
            match e {
                None => {
                    // we have not seen this prefix before
                    let entry = RoaHistoryEntry{
                        tal: entry.tal.clone(),
                        prefix: entry.prefix,
                        max_len: entry.max_len,
                        asn: entry.asn as i64,
                        date_ranges: vec![(Bound::Included(entry.date), Bound::Included(entry.date))]
                    };
                    diesel::insert_into(roa_history_2).values(entry).on_conflict_do_nothing().execute(&self.conn).unwrap();
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

                        diesel::update(
                            roa_history_2.filter(prefix.eq(&entry.prefix))
                                .filter(max_len.eq(&entry.max_len))
                                .filter(asn.eq(&(entry.asn as i64)))
                        )
                            .set(date_ranges.eq(new_ranges))
                            .execute(&self.conn).unwrap();
                    }
                }
            }
        }
    }

    pub fn get_history_entry(&self, prefix_net: &IpNetwork, max_len_val: i32, as_number: i64) -> Option<RoaHistoryEntry> {
        use crate::roa_history_2::dsl::*;

        match roa_history_2.find((prefix_net, as_number, max_len_val)).first::<RoaHistoryEntry>(&self.conn) {
            Ok(entry) => Some(entry),
            Err(_) => None
        }
    }

    pub fn get_all(&self) -> Vec<RoaHistoryEntry> {
        use crate::roa_history_2::dsl::*;
        let res = roa_history_2.load::<RoaHistoryEntry>(&self.conn).unwrap();
        res
    }

    pub fn get_all_files(&self, tal_str: &str, only_unprocessed: bool, reversed: bool) -> Vec<RoaFile> {
        use crate::roa_files_2::dsl::*;
        let mut files = if only_unprocessed {
            roa_files_2
                .filter(tal.eq(tal_str))
                .load::<RoaFile>(&self.conn).unwrap()
        } else {
            roa_files_2.filter(tal.eq(tal_str)).load::<RoaFile>(&self.conn).unwrap()
        };

        files.sort_by(|a, b| a.file_date.partial_cmp(&b.file_date).unwrap());
        if reversed {
            files.reverse();
        }

        files
    }

    pub fn mark_file_as_processed(&self, file_url: &str, processed_v: bool, rows_count_v: i32) {
        use crate::roa_files_2::dsl::*;
        diesel::update(roa_files_2.filter(url.eq(&file_url)))
            .set((processed.eq(processed_v), rows_count.eq(rows_count_v)))
            .execute(&self.conn).unwrap();
    }

    pub fn delete_file(&self, file_url: &str) {
        use crate::roa_files_2::dsl::*;
        diesel::delete(roa_files_2.filter(url.eq(file_url))).execute(&self.conn).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use tracing::{info, Level};
    use crate::{crawl_tal, parse_roas_csv};
    use super::*;

    #[test]
    fn test_connection() {
        let _conn = DbConnection::new();
    }

    #[test]
    fn test_insert_files() {
        tracing_subscriber::fmt().with_max_level(Level::INFO).init();
        let roa_files_2 = crawl_tal("https://ftp.ripe.net/rpki/afrinic.tal", false);

        let conn = DbConnection::new();
        conn.insert_roa_files_2(&roa_files_2);
    }

    #[test]
    fn test_get_all_entry() {
        let conn = DbConnection::new();
        let entries = conn.get_all();
        dbg!(&entries);
    }

    #[test]
    fn test_insert() {
        tracing_subscriber::fmt().with_max_level(Level::INFO).init();
        info!("start");
        let roas = parse_roas_csv("https://ftp.ripe.net/rpki/afrinic.tal/2022/02/01/roas.csv");
        info!("{}", roas.len());
        let conn = DbConnection::new();
        conn.insert_roa_entries(&roas);
        info!("end");
    }

    #[test]
    fn test_find_files() {
        tracing_subscriber::fmt().with_max_level(Level::INFO).init();
        info!("start");
        let conn = DbConnection::new();
        let files = conn.get_all_files("afrinic", false, false);
        for f in files {
            dbg!(f);
        }
        info!("end");
    }

    #[test]
    fn test_processed() {
        tracing_subscriber::fmt().with_max_level(Level::INFO).init();
        let conn = DbConnection::new();
        let roas = parse_roas_csv("https://ftp.ripe.net/rpki/afrinic.tal/2022/02/01/roas.csv");
        conn.mark_file_as_processed("https://ftp.ripe.net/rpki/afrinic.tal/2022/02/01/roas.csv", true, roas.len() as i32);
    }

    #[test]
    fn test_unprocessed() {
        tracing_subscriber::fmt().with_max_level(Level::INFO).init();
        let conn = DbConnection::new();
        conn.mark_file_as_processed("https://ftp.ripe.net/rpki/afrinic.tal/2022/02/01/roas.csv", false, 0);
    }
}