use crate::{parse_roas_csv, RoaEntry};
use anyhow::Result;
use bincode::{Decode, Encode};
use chrono::NaiveDate;
use ipnet::IpNet;
use ipnet_trie::IpnetTrie;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use tabled::Tabled;
use tracing::info;

const KNOWN_GAPS_STR: [(&str, &str); 25] = [
    ("2018-12-28", "2019-01-02"),
    ("2019-10-22", "2019-10-22"),
    ("2019-11-24", "2019-11-24"),
    ("2020-08-03", "2020-08-03"),
    ("2021-01-04", "2021-01-04"),
    ("2021-07-15", "2021-07-15"),
    ("2021-07-19", "2021-07-19"),
    ("2021-07-23", "2021-07-23"),
    ("2021-07-31", "2021-07-31"),
    ("2021-08-10", "2021-08-10"),
    ("2021-09-03", "2021-09-03"),
    ("2021-09-06", "2021-09-07"),
    ("2021-09-10", "2021-09-25"),
    ("2021-09-27", "2021-09-28"),
    ("2022-01-03", "2022-01-03"),
    ("2022-01-15", "2022-01-15"),
    ("2022-01-19", "2022-01-19"),
    ("2022-01-24", "2022-01-24"),
    ("2022-02-02", "2022-02-02"),
    ("2022-02-04", "2022-02-04"),
    ("2022-02-13", "2022-02-13"),
    ("2022-02-16", "2022-02-16"),
    ("2023-06-24", "2023-06-24"),
    ("2023-07-14", "2023-07-17"),
    ("2023-06-24", "2023-06-24"),
];

#[derive(Clone)]
pub struct RoasTrie {
    pub trie: IpnetTrie<HashMap<(u8, u32), RoasTrieEntry>>,
    pub latest_date: i64,
}

#[derive(Debug, Clone, Eq, PartialEq, Encode, Decode)]
pub struct RoasTrieEntry {
    /// ROA max length
    max_len: u8,
    /// Prefix origin
    origin: u32,
    /// Uncompressed dates stored in HashSet
    dates: HashSet<i64>,
    /// Compressed dates stored in VecDeque, each tuple represents a date range
    dates_compressed: VecDeque<(i64, i64)>,
}

#[derive(Debug, Clone)]
pub struct RoasLookupEntry {
    pub prefix: IpNet,
    pub origin: u32,
    pub max_len: u8,
    pub dates_ranges: Vec<(NaiveDate, NaiveDate)>,
}

#[derive(Debug, Clone, Tabled)]
pub struct RoasLookupEntryTabled {
    pub origin: u32,
    pub prefix: String,
    pub max_len: u8,
    pub dates_ranges: String,
}

impl From<RoasLookupEntry> for RoasLookupEntryTabled {
    fn from(entry: RoasLookupEntry) -> Self {
        RoasLookupEntryTabled {
            origin: entry.origin,
            prefix: entry.prefix.to_string(),
            max_len: entry.max_len,
            dates_ranges: entry
                .dates_ranges
                .iter()
                .map(|(start, end)| format!("({},{})", start, end))
                .collect::<Vec<String>>()
                .join(", "),
        }
    }
}

impl RoasTrieEntry {
    pub fn new(date_ts: i64, max_len: u8, origin: u32, bootstrap: bool) -> RoasTrieEntry {
        match bootstrap {
            true => RoasTrieEntry {
                max_len,
                origin,
                dates: HashSet::from([date_ts]),
                dates_compressed: VecDeque::new(),
            },

            false => RoasTrieEntry {
                max_len,
                origin,
                dates: HashSet::new(),
                dates_compressed: VecDeque::from([(date_ts, date_ts)]),
            },
        }
    }
    /// Do full compression where we explode the dates_compressed into individual dates and
    /// then compress them again with the new dates
    pub fn full_compress(&mut self) {
        let mut dates = self.dates.iter().copied().collect::<Vec<i64>>();
        self.dates.clear();

        // explode the dates_compressed into individual dates
        for (start, end) in self.dates_compressed.iter() {
            let mut date = *start;
            while date <= *end {
                dates.push(date);
                date += chrono::Duration::days(1).num_seconds();
            }
        }

        // sort the dates
        dates.sort();

        // compress the dates
        let mut compressed = VecDeque::new();
        let mut start = dates[0];
        let mut end = dates[0];
        for d in dates.iter().skip(1) {
            if *d == end + chrono::Duration::days(1).num_seconds() {
                end = *d;
            } else {
                compressed.push_back((start, end));
                start = *d;
                end = *d;
            }
        }
        compressed.push_back((start, end));

        self.dates_compressed = compressed;

        // after the full compression, the dates HashSet should be empty
        assert!(self.dates.is_empty());
    }

    /// Push a new date: if bootstrap is true, push to hashSet, otherwise push to VecDeque
    pub fn push_date(&mut self, date_ts: i64, bootstrap: bool) {
        match bootstrap {
            true => {
                self.dates.insert(date_ts);
            }
            false => {
                if self.dates_compressed.is_empty() {
                    self.dates_compressed.push_back((date_ts, date_ts));
                } else {
                    let (_start, end) = self.dates_compressed.back_mut().unwrap();
                    let next_day_ts = *end + chrono::Duration::days(1).num_seconds();

                    match date_ts.cmp(&next_day_ts) {
                        Ordering::Equal => {
                            *end = date_ts;
                        }
                        Ordering::Greater => {
                            self.dates_compressed.push_back((date_ts, date_ts));
                        }
                        Ordering::Less => {
                            // the date is already in the range or in the past, skip
                        }
                    }
                }
            }
        }
    }

    pub fn contains_date(&self, date_ts: i64) -> bool {
        self.dates.contains(&date_ts)
            || self
                .dates_compressed
                .iter()
                .any(|(start, end)| date_ts >= *start && date_ts <= *end)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RpkiValidation {
    Valid,
    Invalid,
    Unknown,
}

impl RoasTrieEntry {
    fn convert_to_hash_map(self) -> HashMap<(u8, u32), RoasTrieEntry> {
        HashMap::from([((self.max_len, self.origin), self)])
    }
}

impl Default for RoasTrie {
    fn default() -> Self {
        Self::new()
    }
}

impl RoasTrie {
    pub fn new() -> RoasTrie {
        RoasTrie {
            trie: IpnetTrie::new(),
            latest_date: 0,
        }
    }

    pub fn load(path: &str) -> Result<RoasTrie> {
        info!("loading trie from {} ...", path);
        let mut reader = oneio::get_reader(path)?;
        let mut trie: IpnetTrie<HashMap<(u8, u32), RoasTrieEntry>> = IpnetTrie::new();
        trie.import_from_reader(&mut reader)?;
        let mut roas_trie = RoasTrie {
            trie,
            latest_date: 0,
        };
        roas_trie.update_latest_date();
        Ok(roas_trie)
    }

    pub fn fill_gaps(&mut self) {
        info!("filling known gaps...");
        const ONE_DAY_SECONDS: i64 = 86400;

        for (start, end) in KNOWN_GAPS_STR.iter() {
            let start_ts = chrono::NaiveDate::parse_from_str(start, "%Y-%m-%d")
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp();
            let end_ts = chrono::NaiveDate::parse_from_str(end, "%Y-%m-%d")
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp();

            // vector of timestamps from start_ts to end_ts
            let mut dates = Vec::new();
            let mut date = start_ts;
            while date <= end_ts {
                dates.push(date);
                date += ONE_DAY_SECONDS;
            }

            for (_prefix, map) in self.trie.iter_mut() {
                for (_key, entry) in map.iter_mut() {
                    let mut should_compress = false;
                    for i in 0..entry.dates_compressed.len() - 1 {
                        // let (start, end) = entry.dates_compressed[i];
                        if start_ts - ONE_DAY_SECONDS == entry.dates_compressed[i].1
                            && end_ts + ONE_DAY_SECONDS == entry.dates_compressed[i + 1].0
                        {
                            entry.dates.extend(&dates);
                            should_compress = true;
                        }
                    }
                    if should_compress {
                        entry.full_compress();
                    }
                }
            }
        }
        info!("filling known gaps... done");
    }

    fn update_latest_date(&mut self) {
        info!("updating latest date...");
        let mut latest_date = 0;
        for (_prefix, map) in self.trie.iter() {
            for (_key, entry) in map.iter() {
                if let Some(date) = entry.dates.iter().max() {
                    if *date > latest_date {
                        latest_date = *date;
                    }
                }
                if let Some((_, end)) = entry.dates_compressed.back() {
                    if *end > latest_date {
                        latest_date = *end;
                    }
                }
            }
        }
        self.latest_date = latest_date;
    }

    pub fn dump(&self, path: &str) -> Result<()> {
        info!("exporting trie to {} ...", path);
        let mut writer = oneio::get_writer(path)?;
        self.trie.export_to_writer(&mut writer)?;
        Ok(())
    }

    pub fn process_csv(&mut self, path: &str, bootstrap: bool) -> Result<()> {
        let entries = parse_roas_csv(path)?;
        self.process_entries(&entries, bootstrap);
        Ok(())
    }

    pub fn process_entries(&mut self, entries: &Vec<RoaEntry>, bootstrap: bool) {
        for entry in entries {
            let prefix = entry.prefix;
            let max_len = entry.max_len as u8;
            let origin = entry.asn;
            let date_ts = entry
                .date
                .and_time(chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap())
                .and_utc()
                .timestamp();

            match self.trie.exact_match_mut(prefix) {
                Some(v) => {
                    v.entry((max_len, origin))
                        .and_modify(|e| e.push_date(date_ts, bootstrap))
                        .or_insert_with(|| RoasTrieEntry::new(date_ts, max_len, origin, bootstrap));
                }
                None => {
                    self.trie.insert(
                        prefix,
                        RoasTrieEntry::new(date_ts, max_len, origin, bootstrap)
                            .convert_to_hash_map(),
                    );
                }
            };

            if date_ts > self.latest_date {
                self.latest_date = date_ts;
            }
        }
    }

    pub fn get_latest_date(&self) -> NaiveDate {
        chrono::DateTime::from_timestamp(self.latest_date, 0)
            .unwrap()
            .naive_utc()
            .date()
    }

    pub fn compress_dates(&mut self) {
        info!("compressing dates into date ranges...");
        for (_prefix, map) in self.trie.iter_mut() {
            for (_key, entry) in map.iter_mut() {
                entry.full_compress();
            }
        }
    }

    pub fn validate(&self, prefix: &IpNet, origin: u32, date_ts: i64) -> RpkiValidation {
        let mut is_valid = RpkiValidation::Unknown;
        'outer: for matched in self.trie.matches(prefix) {
            for entry in matched.1.values() {
                if entry.origin == origin
                    && entry.max_len >= prefix.prefix_len()
                    && entry.contains_date(date_ts)
                {
                    is_valid = RpkiValidation::Valid;
                    break 'outer;
                }
            }
            is_valid = RpkiValidation::Invalid;
        }

        is_valid
    }

    pub fn lookup_prefix(&self, prefix: &IpNet) -> Vec<RoasLookupEntry> {
        let mut entries = Vec::new();
        for (prefix, map) in self.trie.matches(prefix) {
            for entry in map.values() {
                entries.push(RoasLookupEntry {
                    prefix,
                    origin: entry.origin,
                    max_len: entry.max_len,
                    dates_ranges: entry
                        .dates_compressed
                        .iter()
                        .map(|(start, end)| {
                            (
                                chrono::DateTime::from_timestamp(*start, 0)
                                    .unwrap()
                                    .naive_utc()
                                    .date(),
                                chrono::DateTime::from_timestamp(*end, 0)
                                    .unwrap()
                                    .naive_utc()
                                    .date(),
                            )
                        })
                        .collect(),
                });
            }
        }
        entries
    }

    pub fn search(
        &self,
        prefix: Option<IpNet>,
        origin: Option<u32>,
        max_len: Option<u8>,
        date: Option<NaiveDate>,
        current: Option<bool>,
    ) -> Vec<RoasLookupEntry> {
        let mut entries = Vec::new();

        // prefix filter
        let iter = match prefix {
            Some(prefix) => self.trie.matches(&prefix),
            None => self.trie.iter().collect(),
        };

        let mut only_expired = false;

        let date = match current {
            Some(c) => match c {
                true => Some(self.latest_date),
                false => {
                    only_expired = true;
                    None
                }
            },
            None => date.map(|d| {
                d.and_time(chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap())
                    .and_utc()
                    .timestamp()
            }),
        };

        for (prefix, map) in iter {
            for entry in map.values() {
                if let Some(origin) = origin {
                    if entry.origin != origin {
                        continue;
                    }
                }

                if let Some(max_len) = max_len {
                    if entry.max_len != max_len {
                        continue;
                    }
                }

                if let Some(date) = date {
                    // check if date_ts is within one of the date ranges in entry.dates_compressed
                    if entry
                        .dates_compressed
                        .iter()
                        .all(|(start, end)| date < *start || date > *end)
                    {
                        continue;
                    }
                }

                if only_expired
                    && entry
                        .dates_compressed
                        .iter()
                        .any(|(_, end)| *end >= self.latest_date)
                {
                    // if any of the date ranges is still current, the entry is current, skip
                    continue;
                }

                entries.push(RoasLookupEntry {
                    prefix,
                    origin: entry.origin,
                    max_len: entry.max_len,
                    dates_ranges: entry
                        .dates_compressed
                        .iter()
                        .map(|(start, end)| {
                            (
                                chrono::DateTime::from_timestamp(*start, 0)
                                    .unwrap()
                                    .naive_utc()
                                    .date(),
                                chrono::DateTime::from_timestamp(*end, 0)
                                    .unwrap()
                                    .naive_utc()
                                    .date(),
                            )
                        })
                        .collect(),
                });
            }
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::log::info;

    #[test]
    fn test_bootstrapped() {
        tracing_subscriber::fmt().init();
        info!("loading trie...");
        let mut roas_trie = RoasTrie::load("roas_trie.bin.gz").unwrap();
        info!("loading trie... done");

        info!("compressing trie...");
        roas_trie.compress_dates();
        roas_trie.dump("roas_trie.compressed.bin.gz").unwrap();
        info!("compressing trie... done");
    }

    #[test]
    fn test_lookup() {
        tracing_subscriber::fmt().init();
        info!("loading trie...");
        let roas_trie = RoasTrie::load("roas_trie.bin.gz").unwrap();
        info!("loading trie... done");

        for results in roas_trie.lookup_prefix(&"1.1.1.0/32".parse().unwrap()) {
            info!("{:?}", results);
        }
    }
}
