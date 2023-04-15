use crate::RoaEntry;
use anyhow::Result;
use chrono::{Duration, NaiveDate};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::table;
use ipnetwork::IpNetwork;
use std::collections::Bound;
use std::env;

table! {
    roa_files (tal, file_date) {
        url -> Text,
        tal -> Text,
        file_date -> Date,
        rows_count -> Integer,
        processed -> Bool,
    }
}

#[derive(Debug, Queryable, Insertable)]
#[diesel(table_name = roa_files)]
pub struct RoaFile {
    pub url: String,
    pub tal: String,
    pub file_date: NaiveDate,
    pub rows_count: i32,
    pub processed: bool,
}

table! {
    roa_history (prefix, asn, max_len) {
        tal -> Text,
        prefix -> Cidr,
        asn -> BigInt,
        date_ranges -> Array<Range<Date>>,
        max_len -> Integer,
    }
}

#[derive(Debug, Queryable, Insertable)]
#[diesel(table_name = roa_history)]
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
fn bound_to_date(v: Bound<NaiveDate>, delta: Duration) -> NaiveDate {
    match v {
        Bound::Included(d) => d,
        Bound::Excluded(d) => d + delta,
        _ => panic!("Date cannot be unbounded"),
    }
}

impl DbConnection {
    /// Create a new database connection.
    pub fn new() -> DbConnection {
        dotenv::dotenv().ok();
        let db_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let conn = PgConnection::establish(db_url.as_str()).unwrap();
        DbConnection { conn }
    }

    /// Get the latest ROA file for a given TAL.
    pub fn get_latest_processed_file(&mut self, tal_name: &str) -> Result<RoaFile> {
        use self::roa_files::dsl::*;
        let file = roa_files
            .filter(tal.eq(tal_name))
            .filter(processed.eq(true))
            .order(file_date.desc())
            .first::<RoaFile>(&mut self.conn)?;
        Ok(file)
    }

    pub fn insert_roa_files(&mut self, files: &Vec<RoaFile>) {
        use self::roa_files::dsl::*;
        diesel::insert_into(roa_files)
            .values(files)
            .on_conflict_do_nothing()
            .execute(&mut self.conn)
            .unwrap();
    }

    pub fn insert_roa_history_entries(&mut self, entries: &Vec<RoaHistoryEntry>) {
        use crate::roa_history::dsl::*;
        entries.chunks(5000).for_each(|chunk| {
            diesel::insert_into(roa_history)
                .values(chunk)
                .on_conflict_do_nothing()
                .execute(&mut self.conn)
                .unwrap();
        });
    }

    pub fn insert_roa_entries<'a>(&mut self, entries: impl IntoIterator<Item = &'a RoaEntry>) {
        use crate::roa_history::dsl::*;

        for entry in entries {
            let e = self.get_history_entry(&entry.prefix, entry.max_len, entry.asn as i64);
            match e {
                None => {
                    // we have not seen this prefix before
                    let entry = RoaHistoryEntry {
                        tal: entry.tal.clone(),
                        prefix: entry.prefix,
                        max_len: entry.max_len,
                        asn: entry.asn as i64,
                        date_ranges: vec![(
                            Bound::Included(entry.date),
                            Bound::Included(entry.date),
                        )],
                    };
                    diesel::insert_into(roa_history)
                        .values(entry)
                        .on_conflict_do_nothing()
                        .execute(&mut self.conn)
                        .unwrap();
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
                            } else if entry.date >= begin_date && entry.date <= end_date {
                                // in between a existing range, skip
                                found = true;
                                // no need to do any db operation
                                skip_update = true;
                            }
                            new_ranges
                                .push((Bound::Included(begin_date), Bound::Included(end_date)));
                        } else {
                            new_ranges.push((begin, end))
                        }
                    }

                    if !found {
                        // non of the existing range can cover the entry, create a new one
                        new_ranges.push((Bound::Included(entry.date), Bound::Included(entry.date)));
                        new_ranges.sort_by(|a, b| {
                            let d_a = bound_to_date(a.0, Duration::days(0));
                            let d_b = bound_to_date(b.0, Duration::days(0));
                            d_a.partial_cmp(&d_b).unwrap()
                        });
                    }

                    if !skip_update {
                        diesel::update(
                            roa_history
                                .filter(prefix.eq(&entry.prefix))
                                .filter(max_len.eq(&entry.max_len))
                                .filter(asn.eq(&(entry.asn as i64))),
                        )
                        .set(date_ranges.eq(new_ranges))
                        .execute(&mut self.conn)
                        .unwrap();
                    }
                }
            }
        }
    }

    pub fn get_history_entry(
        &mut self,
        prefix_net: &IpNetwork,
        max_len_val: i32,
        as_number: i64,
    ) -> Option<RoaHistoryEntry> {
        use crate::roa_history::dsl::*;

        match roa_history
            .find((prefix_net, as_number, max_len_val))
            .first::<RoaHistoryEntry>(&mut self.conn)
        {
            Ok(entry) => Some(entry),
            Err(_) => None,
        }
    }

    pub fn get_all(&mut self) -> Vec<RoaHistoryEntry> {
        use crate::roa_history::dsl::*;
        let res = roa_history.load::<RoaHistoryEntry>(&mut self.conn).unwrap();
        res
    }

    /// Get all the files for a given TAL
    /// If only_unprocessed is true, only return the files that have not been processed yet
    pub fn get_all_files(
        &mut self,
        tal_str: &str,
        only_unprocessed: bool,
        reversed: bool,
    ) -> Vec<RoaFile> {
        use crate::roa_files::dsl::*;

        let mut files = if only_unprocessed {
            roa_files
                .filter(tal.eq(tal_str))
                .filter(processed.eq(false))
                .load::<RoaFile>(&mut self.conn)
                .unwrap()
        } else {
            roa_files
                .filter(tal.eq(tal_str))
                .load::<RoaFile>(&mut self.conn)
                .unwrap()
        };

        files.sort_by(|a, b| a.file_date.partial_cmp(&b.file_date).unwrap());
        if reversed {
            files.reverse();
        }

        files
    }

    pub fn mark_file_as_processed(&mut self, file_url: &str, processed_v: bool, rows_count_v: i32) {
        use crate::roa_files::dsl::*;
        diesel::update(roa_files.filter(url.eq(&file_url)))
            .set((processed.eq(processed_v), rows_count.eq(rows_count_v)))
            .execute(&mut self.conn)
            .unwrap();
    }

    pub fn delete_file(&mut self, file_url: &str) {
        use crate::roa_files::dsl::*;
        diesel::delete(roa_files.filter(url.eq(file_url)))
            .execute(&mut self.conn)
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{crawl_tal, parse_roas_csv};
    use tracing::{info, Level};

    #[test]
    fn test_connection() {
        let _conn = DbConnection::new();
    }

    #[test]
    fn test_insert_files() {
        tracing_subscriber::fmt().with_max_level(Level::INFO).init();
        let roa_files = crawl_tal("https://ftp.ripe.net/rpki/afrinic.tal", false);

        let mut conn = DbConnection::new();
        conn.insert_roa_files(&roa_files);
    }

    #[test]
    fn test_get_all_entry() {
        let mut conn = DbConnection::new();
        let entries = conn.get_all();
        dbg!(&entries);
    }

    #[test]
    fn test_insert() {
        tracing_subscriber::fmt().with_max_level(Level::INFO).init();
        info!("start");
        let roas = parse_roas_csv("https://ftp.ripe.net/rpki/afrinic.tal/2022/02/01/roas.csv");
        info!("{}", roas.len());
        let mut conn = DbConnection::new();
        conn.insert_roa_entries(&roas);
        info!("end");
    }

    #[test]
    fn test_find_files() {
        tracing_subscriber::fmt().with_max_level(Level::INFO).init();
        info!("start");
        let mut conn = DbConnection::new();
        let files = conn.get_all_files("afrinic", false, false);
        for f in files {
            dbg!(f);
        }
        info!("end");
    }

    #[test]
    fn test_processed() {
        tracing_subscriber::fmt().with_max_level(Level::INFO).init();
        let mut conn = DbConnection::new();
        let roas = parse_roas_csv("https://ftp.ripe.net/rpki/afrinic.tal/2022/02/01/roas.csv");
        conn.mark_file_as_processed(
            "https://ftp.ripe.net/rpki/afrinic.tal/2022/02/01/roas.csv",
            true,
            roas.len() as i32,
        );
    }

    #[test]
    fn test_unprocessed() {
        tracing_subscriber::fmt().with_max_level(Level::INFO).init();
        let mut conn = DbConnection::new();
        conn.mark_file_as_processed(
            "https://ftp.ripe.net/rpki/afrinic.tal/2022/02/01/roas.csv",
            false,
            0,
        );
    }
}
