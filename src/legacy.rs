//! Legacy v1 archive support (bincode + ipnet-trie).
//!
//! Users pointing at a `.bin`/`.bin.gz` data file are routed to this
//! in-memory implementation, preserving v1 behavior. `convert` uses
//! [`LegacyRoasTrie::load`] followed by conversion to the v2 builder.

use crate::roas_trie::KNOWN_GAPS_STR;
use crate::{
    crawl_tal_after, get_tal_urls, parse_roas_csv, RoaEntry, RoaFile, RoaRecordMut,
    RoasLookupEntry, RoasTrieMut, RpkiValidation,
};
use anyhow::Result;
use bincode::{Decode, Encode};
use chrono::NaiveDate;
use ipnet::IpNet;
use ipnet_trie::IpnetTrie;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use tracing::info;

/// Type alias for trie entries keyed by (max_len, origin).
type RoasTrieMap = HashMap<(u8, u32), LegacyRoasTrieEntry>;

#[derive(Clone)]
pub struct LegacyRoasTrie {
    pub(crate) trie: IpnetTrie<HashMap<(u8, u32), LegacyRoasTrieEntry>>,
    pub(crate) latest_date: i64,
}

#[derive(Debug, Clone, Eq, PartialEq, Encode, Decode)]
pub struct LegacyRoasTrieEntry {
    /// ROA max length
    max_len: u8,
    /// Prefix origin
    origin: u32,
    /// Uncompressed dates stored in HashSet
    dates: HashSet<i64>,
    /// Compressed dates stored in VecDeque, each tuple represents a date range
    dates_compressed: VecDeque<(i64, i64)>,
}

impl LegacyRoasTrieEntry {
    pub fn new(date_ts: i64, max_len: u8, origin: u32, bootstrap: bool) -> LegacyRoasTrieEntry {
        match bootstrap {
            true => LegacyRoasTrieEntry {
                max_len,
                origin,
                dates: HashSet::from([date_ts]),
                dates_compressed: VecDeque::new(),
            },

            false => LegacyRoasTrieEntry {
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

impl LegacyRoasTrieEntry {
    fn convert_to_hash_map(self) -> HashMap<(u8, u32), LegacyRoasTrieEntry> {
        HashMap::from([((self.max_len, self.origin), self)])
    }
}

impl Default for LegacyRoasTrie {
    fn default() -> Self {
        Self::new()
    }
}

impl LegacyRoasTrie {
    pub fn new() -> LegacyRoasTrie {
        LegacyRoasTrie {
            trie: IpnetTrie::new(),
            latest_date: 0,
        }
    }

    pub fn load(path: &str) -> Result<LegacyRoasTrie> {
        info!("loading trie from {} ...", path);
        let mut reader = oneio::get_reader(path)?;
        let mut trie: IpnetTrie<HashMap<(u8, u32), LegacyRoasTrieEntry>> = IpnetTrie::new();
        trie.import_from_reader(&mut reader)?;
        let mut roas_trie = LegacyRoasTrie {
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
                for entry in map.values_mut() {
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
        let mut latest_date = 0;
        for (_prefix, map) in self.trie.iter() {
            for entry in map.values() {
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
        info!("updating latest date to {}", self.get_latest_date());
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
                        .or_insert_with(|| {
                            LegacyRoasTrieEntry::new(date_ts, max_len, origin, bootstrap)
                        });
                }
                None => {
                    self.trie.insert(
                        prefix,
                        LegacyRoasTrieEntry::new(date_ts, max_len, origin, bootstrap)
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
            for entry in map.values_mut() {
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
        exact: bool,
    ) -> Vec<RoasLookupEntry> {
        let mut entries = Vec::new();

        // prefix filter: exact match by default (fixes issue #9),
        // falls back to matches() when exact=false for supernet/subnet inclusion
        let iter: Vec<(IpNet, &RoasTrieMap)> = match prefix {
            Some(prefix) if exact => match self.trie.exact_match(prefix) {
                Some(map) => vec![(prefix, map)],
                None => vec![],
            },
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

    pub fn update(&mut self, tal: Option<String>, until: Option<NaiveDate>) -> Result<()> {
        info!("updating trie... tal: {:?}, until: {:?}", &tal, &until);
        let mut all_files = get_tal_urls(tal)
            .into_iter()
            .flat_map(|tal_url| {
                crawl_tal_after(
                    tal_url.as_str(),
                    Some(self.get_latest_date() + chrono::Duration::days(1)),
                    until,
                )
            })
            .collect::<Vec<RoaFile>>();

        if all_files.is_empty() {
            info!("trie is up to date. No new files found.");
            return Ok(());
        }

        // sort by date
        all_files.sort_by_key(|a| a.file_date);

        for file in all_files {
            info!("processing {}", file.url.as_str());
            let url: &str = file.url.as_str();
            if let Ok(roas) = parse_roas_csv(url) {
                self.process_entries(&roas, false);
            }
        }

        info!("updating trie... done");
        Ok(())
    }

    pub fn replace(&mut self, other: LegacyRoasTrie) {
        self.trie = other.trie;
        self.latest_date = other.latest_date;
    }

    /// Convert the v1 in-memory trie into a v2 [`RoasTrieMut`] builder,
    /// preserving all prefix/origin/max_len/date-range data.
    ///
    /// This is used by:
    /// - the `convert` subcommand
    /// - automatic on-the-fly conversion when a `.rkyv` file is missing but a
    ///   sibling `.bin`/`.bin.gz` file exists
    pub fn to_builder(&self) -> RoasTrieMut {
        let mut builder = RoasTrieMut::new();
        for (prefix, entries) in self.trie.iter() {
            for ((max_len, origin), entry) in entries.iter() {
                // Collect all date ranges (both uncompressed HashSet and compressed VecDeque)
                let mut all_dates: Vec<i64> = entry.dates.iter().copied().collect();
                for (start, end) in entry.dates_compressed.iter() {
                    let mut d = *start;
                    while d <= *end {
                        all_dates.push(d);
                        d += chrono::Duration::days(1).num_seconds();
                    }
                }
                all_dates.sort();
                all_dates.dedup();

                // Build ranges from sorted dates
                let mut ranges = Vec::new();
                if !all_dates.is_empty() {
                    let mut range_start = all_dates[0];
                    let mut range_end = all_dates[0];
                    for &d in &all_dates[1..] {
                        if d == range_end + chrono::Duration::days(1).num_seconds() {
                            range_end = d;
                        } else {
                            ranges.push((range_start, range_end));
                            range_start = d;
                            range_end = d;
                        }
                    }
                    ranges.push((range_start, range_end));
                }

                let record = RoaRecordMut {
                    origin: *origin,
                    max_len: *max_len,
                    dates: HashSet::new(),
                    ranges,
                };
                builder.insert_record(prefix, record);
            }
        }
        builder.set_latest_date(self.latest_date);
        builder
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RoaEntry;
    use chrono::NaiveDate;
    use ipnet::IpNet;
    use std::net::Ipv4Addr;
    use std::str::FromStr;

    fn make_entry(prefix: &str, asn: u32, max_len: i32, date: NaiveDate) -> RoaEntry {
        RoaEntry {
            tal: "test".to_string(),
            prefix: IpNet::from_str(prefix).unwrap(),
            max_len,
            asn,
            date,
        }
    }

    fn build_test_trie() -> LegacyRoasTrie {
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let entries = vec![
            // supernet
            make_entry("1.0.0.0/8", 64512, 8, date),
            // exact prefix we will search for
            make_entry("1.1.1.0/24", 13335, 24, date),
            // subnet (more-specific)
            make_entry("1.1.1.0/25", 64513, 25, date),
        ];
        let mut trie = LegacyRoasTrie::new();
        trie.process_entries(&entries, true);
        trie.compress_dates();
        trie
    }

    #[test]
    fn test_search_exact_returns_only_matching_prefix() {
        let trie = build_test_trie();
        let prefix = IpNet::V4(ipnet::Ipv4Net::new(Ipv4Addr::new(1, 1, 1, 0), 24).unwrap());

        // exact=true should return only the /24 ROA
        let results = trie.search(Some(prefix), None, None, None, None, true);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].prefix.to_string(), "1.1.1.0/24");
        assert_eq!(results[0].origin, 13335);
    }

    #[test]
    fn test_search_non_exact_includes_super_and_sub() {
        let trie = build_test_trie();
        let prefix = IpNet::V4(ipnet::Ipv4Net::new(Ipv4Addr::new(1, 1, 1, 0), 24).unwrap());

        // exact=false should use matches(), which includes the supernet and subnet
        let results = trie.search(Some(prefix), None, None, None, None, false);
        let prefixes: Vec<String> = results.iter().map(|r| r.prefix.to_string()).collect();
        assert!(prefixes.contains(&"1.0.0.0/8".to_string()));
        assert!(prefixes.contains(&"1.1.1.0/24".to_string()));
        assert!(prefixes.contains(&"1.1.1.0/25".to_string()));
    }

    #[test]
    fn test_search_exact_no_match_returns_empty() {
        let trie = build_test_trie();
        // prefix not in trie
        let prefix = IpNet::V4(ipnet::Ipv4Net::new(Ipv4Addr::new(2, 2, 2, 0), 24).unwrap());

        let results = trie.search(Some(prefix), None, None, None, None, true);
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_no_prefix_returns_all() {
        let trie = build_test_trie();

        // No prefix filter → return all entries regardless of exact flag
        let results = trie.search(None, None, None, None, None, true);
        assert_eq!(results.len(), 3);
    }
}
