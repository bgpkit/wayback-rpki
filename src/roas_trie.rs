use crate::{parse_roas_csv, RoaEntry};
use anyhow::Result;
use bincode::{Decode, Encode};
use ipnet::IpNet;
use ipnet_trie::IpnetTrie;
use std::collections::{HashMap, HashSet, VecDeque};

pub struct RoasTrie {
    pub trie: IpnetTrie<HashMap<(u8, u32), RoasTrieEntry>>,
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

impl RoasTrieEntry {
    pub fn new_from_date(date_ts: i64, bootstrap: bool) -> RoasTrieEntry {
        match bootstrap {
            true => RoasTrieEntry {
                max_len: 0,
                origin: 0,
                dates: HashSet::from([date_ts]),
                dates_compressed: VecDeque::new(),
            },

            false => RoasTrieEntry {
                max_len: 0,
                origin: 0,
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

        // after the full compression, the dates HahsSet should be empty
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
                    if *end + chrono::Duration::days(1).num_seconds() == date_ts {
                        *end = date_ts;
                    } else {
                        self.dates_compressed.push_back((date_ts, date_ts));
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
        }
    }

    pub fn load(path: &str) -> Result<RoasTrie> {
        let mut reader = oneio::get_reader(path)?;
        let mut trie: IpnetTrie<HashMap<(u8, u32), RoasTrieEntry>> = IpnetTrie::new();
        trie.import_from_reader(&mut reader)?;
        Ok(RoasTrie { trie })
    }

    pub fn dump(&self, path: &str) -> Result<()> {
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
                        .or_insert_with(|| RoasTrieEntry::new_from_date(date_ts, bootstrap));
                }
                None => {
                    self.trie.insert(
                        prefix,
                        RoasTrieEntry::new_from_date(date_ts, bootstrap).convert_to_hash_map(),
                    );
                }
            };
        }
    }

    pub fn compress_dates(&mut self) {
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
}
