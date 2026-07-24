use crate::{crawl_tal_after, get_tal_urls, parse_roas_csv, RoaEntry, RoaFile};
use anyhow::{anyhow, Result};
use chrono::NaiveDate;
use ipnet::IpNet;
use prefix_trie::joint::JointPrefixMap;
use prefix_trie::{AsView, TrieView};
use rkyv::{Archive, Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Read;
use tabled::Tabled;
use tracing::{info, warn};

/// On-disk format version. v2: rkyv archive of [`RoasTrieData`].
pub const FORMAT_VERSION: u32 = 2;

/// Default remote bootstrap archive URL (v2 rkyv format).
pub const REMOTE_BOOTSTRAP_URL: &str = "https://spaces.bgpkit.org/broker/roas_trie.rkyv";

pub(crate) const KNOWN_GAPS_STR: [(&str, &str); 24] = [
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
];

const ONE_DAY_SECONDS: i64 = 86400;

/// A single ROA record in the serialized archive: one (max_len, origin) pair
/// with its compressed date ranges.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
pub struct RoaRecord {
    /// ROA max length
    pub max_len: u8,
    /// ROA origin ASN
    pub origin: u32,
    /// Compressed date ranges, each tuple is (start_ts, end_ts), UTC day granularity.
    pub dates: Vec<(i64, i64)>,
}

/// Mutable per-ROA record used while building or updating the trie.
#[derive(Debug, Clone)]
pub struct RoaRecordMut {
    pub max_len: u8,
    pub origin: u32,
    /// Uncompressed dates collected during bootstrap.
    #[allow(dead_code)]
    pub(crate) dates: HashSet<i64>,
    /// Compressed date ranges.
    pub(crate) ranges: Vec<(i64, i64)>,
}

impl RoaRecordMut {
    /// Create an empty record for legacy import.
    #[allow(dead_code)]
    pub(crate) fn new_legacy(max_len: u8, origin: u32) -> Self {
        RoaRecordMut {
            max_len,
            origin,
            dates: HashSet::new(),
            ranges: Vec::new(),
        }
    }

    fn new(date_ts: i64, max_len: u8, origin: u32, bootstrap: bool) -> Self {
        if bootstrap {
            RoaRecordMut {
                max_len,
                origin,
                dates: HashSet::from([date_ts]),
                ranges: Vec::new(),
            }
        } else {
            RoaRecordMut {
                max_len,
                origin,
                dates: HashSet::new(),
                ranges: vec![(date_ts, date_ts)],
            }
        }
    }

    fn push_date(&mut self, date_ts: i64, bootstrap: bool) {
        if bootstrap {
            self.dates.insert(date_ts);
            return;
        }
        match self.ranges.last_mut() {
            None => self.ranges.push((date_ts, date_ts)),
            Some((_, end)) => {
                let next_day_ts = *end + ONE_DAY_SECONDS;
                if date_ts == next_day_ts {
                    *end = date_ts;
                } else if date_ts > next_day_ts {
                    self.ranges.push((date_ts, date_ts));
                }
                // date_ts <= end: already covered, skip
            }
        }
    }

    /// Merge `dates` into `ranges`, producing the final compressed ranges.
    fn full_compress(&mut self) {
        let mut all: Vec<i64> = self.dates.iter().copied().collect();
        self.dates.clear();
        for (start, end) in self.ranges.iter() {
            let mut d = *start;
            while d <= *end {
                all.push(d);
                d += ONE_DAY_SECONDS;
            }
        }
        all.sort_unstable();

        let mut compressed: Vec<(i64, i64)> = Vec::new();
        if all.is_empty() {
            self.ranges = compressed;
            return;
        }
        let mut start = all[0];
        let mut end = all[0];
        for d in all.iter().skip(1) {
            if *d == end + ONE_DAY_SECONDS {
                end = *d;
            } else {
                compressed.push((start, end));
                start = *d;
                end = *d;
            }
        }
        compressed.push((start, end));
        self.ranges = compressed;
    }
}

/// The serialized archive: header metadata plus the prefix trie.
#[derive(Archive, Serialize, Deserialize)]
pub struct RoasTrieData {
    pub format_version: u32,
    pub latest_date: i64,
    pub ipv4_count: u64,
    pub ipv6_count: u64,
    pub trie: JointPrefixMap<IpNet, Vec<RoaRecord>>,
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

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RpkiValidation {
    Valid,
    Invalid,
    Unknown,
}

impl std::fmt::Display for RpkiValidation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpkiValidation::Valid => write!(f, "valid"),
            RpkiValidation::Invalid => write!(f, "invalid"),
            RpkiValidation::Unknown => write!(f, "unknown"),
        }
    }
}

pub(crate) fn ts_to_date(ts: i64) -> NaiveDate {
    chrono::DateTime::from_timestamp(ts, 0)
        .unwrap()
        .naive_utc()
        .date()
}

pub(crate) fn date_to_ts(date: NaiveDate) -> i64 {
    date.and_time(chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap())
        .and_utc()
        .timestamp()
}

/// Builder/mutable trie for rebuild, update, and fix operations.
pub struct RoasTrieMut {
    trie: JointPrefixMap<IpNet, Vec<RoaRecordMut>>,
    latest_date: i64,
}

impl Default for RoasTrieMut {
    fn default() -> Self {
        Self::new()
    }
}

impl RoasTrieMut {
    pub fn new() -> Self {
        RoasTrieMut {
            trie: JointPrefixMap::new(),
            latest_date: 0,
        }
    }

    /// Load a v2 archive from disk into a mutable trie for updating.
    pub fn load(path: &str) -> Result<Self> {
        info!("loading trie from {} ...", path);
        let mut bytes = Vec::new();
        oneio::get_reader(path)?.read_to_end(&mut bytes)?;
        let data: RoasTrieData = rkyv::from_bytes::<_, rkyv::rancor::Error>(&bytes)
            .map_err(|e| anyhow!("failed to deserialize trie archive: {}", e))?;
        if data.format_version != FORMAT_VERSION {
            return Err(anyhow!(
                "unsupported trie format version {} (expected {})",
                data.format_version,
                FORMAT_VERSION
            ));
        }
        let mut trie: JointPrefixMap<IpNet, Vec<RoaRecordMut>> = JointPrefixMap::new();
        for (prefix, records) in data.trie.iter() {
            let recs: Vec<RoaRecordMut> = records
                .iter()
                .map(|r| RoaRecordMut {
                    max_len: r.max_len,
                    origin: r.origin,
                    dates: HashSet::new(),
                    ranges: r.dates.clone(),
                })
                .collect();
            trie.insert(prefix, recs);
        }
        info!(
            "loaded {} prefixes, latest date {}",
            trie.len(),
            ts_to_date(data.latest_date)
        );
        Ok(RoasTrieMut {
            trie,
            latest_date: data.latest_date,
        })
    }

    /// Compress all in-flight dates and serialize the trie to `path` (atomically).
    pub fn dump(&mut self, path: &str) -> Result<()> {
        info!("compressing dates into date ranges...");
        self.compress_dates();

        let mut v4_count: u64 = 0;
        let mut v6_count: u64 = 0;
        let mut out: JointPrefixMap<IpNet, Vec<RoaRecord>> = JointPrefixMap::new();
        for (prefix, records) in self.trie.iter() {
            match prefix {
                IpNet::V4(_) => v4_count += 1,
                IpNet::V6(_) => v6_count += 1,
            }
            let recs: Vec<RoaRecord> = records
                .iter()
                .map(|r| RoaRecord {
                    max_len: r.max_len,
                    origin: r.origin,
                    dates: r.ranges.clone(),
                })
                .collect();
            out.insert(prefix, recs);
        }

        let data = RoasTrieData {
            format_version: FORMAT_VERSION,
            latest_date: self.latest_date,
            ipv4_count: v4_count,
            ipv6_count: v6_count,
            trie: out,
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&data)
            .map_err(|e| anyhow!("failed to serialize trie: {}", e))?;

        info!(
            "exporting trie to {} ({} prefixes, {:.1} MB) ...",
            path,
            data.trie.len(),
            bytes.len() as f64 / 1024.0 / 1024.0
        );
        let tmp_path = format!("{}.tmp", path);
        std::fs::write(&tmp_path, &bytes)?;
        std::fs::rename(&tmp_path, path)?;
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
            let date_ts = date_to_ts(entry.date);

            match self.trie.get_mut(&prefix) {
                Some(recs) => match recs
                    .iter_mut()
                    .find(|r| r.max_len == max_len && r.origin == origin)
                {
                    Some(r) => r.push_date(date_ts, bootstrap),
                    None => recs.push(RoaRecordMut::new(date_ts, max_len, origin, bootstrap)),
                },
                None => {
                    self.trie.insert(
                        prefix,
                        vec![RoaRecordMut::new(date_ts, max_len, origin, bootstrap)],
                    );
                }
            }

            if date_ts > self.latest_date {
                self.latest_date = date_ts;
            }
        }
    }

    pub fn get_latest_date(&self) -> NaiveDate {
        ts_to_date(self.latest_date)
    }

    fn compress_dates(&mut self) {
        for (_prefix, records) in self.trie.iter_mut() {
            for r in records.iter_mut() {
                r.full_compress();
            }
        }
    }

    /// Incremental update: crawl new ROA files since `latest_date` and apply.
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

        all_files.sort_by_key(|a| a.file_date);

        let mut failed = 0usize;
        for file in &all_files {
            info!("processing {}", file.url.as_str());
            match parse_roas_csv(file.url.as_str()) {
                Ok(roas) => self.process_entries(&roas, false),
                Err(e) => {
                    failed += 1;
                    warn!("failed to process {}: {}", file.url, e);
                }
            }
        }
        if failed > 0 {
            warn!(
                "update finished with {} failed file(s) out of {}",
                failed,
                all_files.len()
            );
        }

        info!("updating trie... done");
        Ok(())
    }

    /// Fill known historical data gaps by interpolating adjacent date ranges.
    pub fn fill_gaps(&mut self) {
        info!("filling known gaps...");
        for (start, end) in KNOWN_GAPS_STR.iter() {
            let start_ts = date_to_ts(NaiveDate::parse_from_str(start, "%Y-%m-%d").unwrap());
            let end_ts = date_to_ts(NaiveDate::parse_from_str(end, "%Y-%m-%d").unwrap());

            let mut dates = Vec::new();
            let mut d = start_ts;
            while d <= end_ts {
                dates.push(d);
                d += ONE_DAY_SECONDS;
            }

            for (_prefix, records) in self.trie.iter_mut() {
                for r in records.iter_mut() {
                    let mut should_compress = false;
                    for i in 0..r.ranges.len().saturating_sub(1) {
                        if start_ts - ONE_DAY_SECONDS == r.ranges[i].1
                            && end_ts + ONE_DAY_SECONDS == r.ranges[i + 1].0
                        {
                            r.dates.extend(&dates);
                            should_compress = true;
                        }
                    }
                    if should_compress {
                        r.full_compress();
                    }
                }
            }
        }
        info!("filling known gaps... done");
    }

    pub fn len(&self) -> usize {
        self.trie.len()
    }

    pub fn is_empty(&self) -> bool {
        self.trie.is_empty()
    }

    /// Insert a single record under `prefix`.
    pub(crate) fn insert_record(&mut self, prefix: IpNet, record: RoaRecordMut) {
        match self.trie.get_mut(&prefix) {
            Some(recs) => recs.push(record),
            None => {
                self.trie.insert(prefix, vec![record]);
            }
        }
    }

    /// Set the latest date.
    pub(crate) fn set_latest_date(&mut self, ts: i64) {
        self.latest_date = ts;
    }
}

/// Read-only trie backed by an rkyv archive, either memory-mapped from disk
/// or held as owned bytes. Queries run directly on the archived bytes without
/// deserializing the trie into heap structures.
pub struct RoasTrie {
    bytes: TrieBytes,
}

enum TrieBytes {
    Mmap(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl TrieBytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            TrieBytes::Mmap(m) => m.as_ref(),
            TrieBytes::Owned(v) => v.as_slice(),
        }
    }
}

impl RoasTrie {
    /// Open a v2 archive file read-only via memory map. The file is validated
    /// once on open; queries then run directly against the mapped bytes.
    pub fn open(path: &str) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        // SAFETY: the file is opened read-only and the mapping is never mutated.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let trie = RoasTrie {
            bytes: TrieBytes::Mmap(mmap),
        };
        trie.validate_bytes()?;
        Ok(trie)
    }

    /// Create a read handle from owned archive bytes (e.g. freshly serialized).
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let trie = RoasTrie {
            bytes: TrieBytes::Owned(bytes),
        };
        trie.validate_bytes()?;
        Ok(trie)
    }

    fn validate_bytes(&self) -> Result<()> {
        let data: &ArchivedRoasTrieData =
            rkyv::access::<_, rkyv::rancor::Error>(self.bytes.as_slice())
                .map_err(|e| anyhow!("trie archive validation failed: {}", e))?;
        if data.format_version.to_native() != FORMAT_VERSION {
            return Err(anyhow!(
                "unsupported trie format version {} (expected {})",
                data.format_version.to_native(),
                FORMAT_VERSION
            ));
        }
        Ok(())
    }

    /// Access the archived data.
    fn data(&self) -> &ArchivedRoasTrieData {
        // SAFETY: the bytes were validated in full at open()/from_bytes() and
        // are immutable for the lifetime of this struct.
        unsafe { rkyv::access_unchecked::<ArchivedRoasTrieData>(self.bytes.as_slice()) }
    }

    pub fn latest_date_ts(&self) -> i64 {
        self.data().latest_date.to_native()
    }

    pub fn get_latest_date(&self) -> NaiveDate {
        ts_to_date(self.latest_date_ts())
    }

    pub fn counts(&self) -> (u64, u64) {
        let data = self.data();
        (data.ipv4_count.to_native(), data.ipv6_count.to_native())
    }

    /// Total number of prefix entries.
    pub fn len(&self) -> usize {
        self.data().trie.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// RPKI validation for a prefix/origin at a given date.
    pub fn validate(&self, prefix: &IpNet, origin: u32, date_ts: i64) -> RpkiValidation {
        let mut result = RpkiValidation::Unknown;
        let prefix_len = prefix.prefix_len();
        for (_p, records) in self.match_records(prefix) {
            for r in records.iter() {
                let r_origin = r.origin.to_native();
                let r_max_len = r.max_len;
                if r_origin == origin && r_max_len >= prefix_len && record_contains_date(r, date_ts)
                {
                    return RpkiValidation::Valid;
                }
            }
            result = RpkiValidation::Invalid;
        }
        result
    }

    /// All ROA records matching a prefix, including supernets and subnets.
    pub fn lookup_prefix(&self, prefix: &IpNet) -> Vec<RoasLookupEntry> {
        let mut entries = Vec::new();
        for (p, records) in self.match_records(prefix) {
            for r in records.iter() {
                entries.push(archived_lookup_entry(p, r));
            }
        }
        entries
    }

    /// Search ROAs with optional filters. `exact=true` restricts to the exact
    /// prefix; `exact=false` includes supernets and subnets (matching the old
    /// `matches()` semantics: the subtree of the shortest covering prefix).
    pub fn search(
        &self,
        prefix: Option<IpNet>,
        origin: Option<u32>,
        max_len: Option<u8>,
        date: Option<NaiveDate>,
        current: Option<bool>,
        exact: bool,
    ) -> Vec<RoasLookupEntry> {
        let mut only_expired = false;
        let date_ts = match current {
            Some(true) => Some(self.latest_date_ts()),
            Some(false) => {
                only_expired = true;
                None
            }
            None => date.map(date_to_ts),
        };
        let latest = self.latest_date_ts();

        let mut entries = Vec::new();
        for (p, records) in self.select_records(prefix, exact) {
            // deterministic order within a prefix (v1 was nondeterministic HashMap order)
            let mut sorted: Vec<&ArchivedRoaRecord> = records.iter().collect();
            sorted.sort_by_key(|r| (r.origin.to_native(), r.max_len));
            for r in sorted {
                if let Some(origin) = origin {
                    if r.origin.to_native() != origin {
                        continue;
                    }
                }
                if let Some(max_len) = max_len {
                    if r.max_len != max_len {
                        continue;
                    }
                }
                if let Some(date_ts) = date_ts {
                    if !record_contains_date(r, date_ts) {
                        continue;
                    }
                }
                if only_expired && r.dates.iter().any(|range| range.1.to_native() >= latest) {
                    continue;
                }
                entries.push(archived_lookup_entry(p, r));
            }
        }
        entries
    }

    /// Select (prefix, records) pairs according to the prefix filter mode.
    fn select_records(
        &self,
        prefix: Option<IpNet>,
        exact: bool,
    ) -> Vec<(IpNet, &ArchivedVec<ArchivedRoaRecord>)> {
        match prefix {
            Some(p) if exact => match self.data().trie.get(&p) {
                Some(recs) => vec![(p, recs)],
                None => vec![],
            },
            Some(p) => self.match_records(&p),
            None => self.data().trie.iter().collect(),
        }
    }

    /// Entries matching `prefix` with old `IpnetTrie::matches()` semantics:
    /// the shortest covering prefix and its entire subtree.
    fn match_records<'a>(
        &'a self,
        prefix: &IpNet,
    ) -> Vec<(IpNet, &'a ArchivedVec<ArchivedRoaRecord>)> {
        let data = self.data();
        let spm = match data.trie.get_spm(prefix) {
            Some((spm, _)) => spm,
            None => return vec![],
        };
        match spm {
            IpNet::V4(spm4) => match (&data.trie.t1).view_at(&spm4) {
                Some(view) => view.iter().map(|(p, recs)| (IpNet::V4(p), recs)).collect(),
                None => vec![],
            },
            IpNet::V6(spm6) => match (&data.trie.t2).view_at(&spm6) {
                Some(view) => view.iter().map(|(p, recs)| (IpNet::V6(p), recs)).collect(),
                None => vec![],
            },
        }
    }
}

fn record_contains_date(r: &ArchivedRoaRecord, date_ts: i64) -> bool {
    r.dates.iter().any(|range| {
        let start = range.0.to_native();
        let end = range.1.to_native();
        date_ts >= start && date_ts <= end
    })
}

fn archived_lookup_entry(prefix: IpNet, r: &ArchivedRoaRecord) -> RoasLookupEntry {
    RoasLookupEntry {
        prefix,
        origin: r.origin.to_native(),
        max_len: r.max_len,
        dates_ranges: r
            .dates
            .iter()
            .map(|range| {
                (
                    ts_to_date(range.0.to_native()),
                    ts_to_date(range.1.to_native()),
                )
            })
            .collect(),
    }
}

// Re-export for convenience of the API layer.
pub use rkyv::vec::ArchivedVec;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(prefix: &str, asn: u32, max_len: i32, date: NaiveDate) -> RoaEntry {
        RoaEntry {
            tal: "test".to_string(),
            prefix: prefix.parse().unwrap(),
            max_len,
            asn,
            date,
        }
    }

    fn build_test_trie(name: &str) -> RoasTrie {
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let entries = vec![
            make_entry("1.0.0.0/8", 64512, 8, date),
            make_entry("1.1.1.0/24", 13335, 24, date),
            make_entry("1.1.1.0/25", 64513, 25, date),
        ];
        let mut trie = RoasTrieMut::new();
        trie.process_entries(&entries, true);
        // serialize in-memory and re-open
        let tmp = std::env::temp_dir().join(format!(
            "wayback-rpki-test-{}-{}.rkyv",
            name,
            std::process::id()
        ));
        trie.dump(tmp.to_str().unwrap()).unwrap();
        let t = RoasTrie::open(tmp.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&tmp);
        t
    }

    #[test]
    fn test_search_exact_returns_only_matching_prefix() {
        let trie = build_test_trie(concat!("t", line!()));
        let prefix: IpNet = "1.1.1.0/24".parse().unwrap();

        let results = trie.search(Some(prefix), None, None, None, None, true);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].prefix.to_string(), "1.1.1.0/24");
        assert_eq!(results[0].origin, 13335);
    }

    #[test]
    fn test_search_non_exact_includes_super_and_sub() {
        let trie = build_test_trie(concat!("t", line!()));
        let prefix: IpNet = "1.1.1.0/24".parse().unwrap();

        let results = trie.search(Some(prefix), None, None, None, None, false);
        let prefixes: Vec<String> = results.iter().map(|r| r.prefix.to_string()).collect();
        assert!(prefixes.contains(&"1.0.0.0/8".to_string()));
        assert!(prefixes.contains(&"1.1.1.0/24".to_string()));
        assert!(prefixes.contains(&"1.1.1.0/25".to_string()));
    }

    #[test]
    fn test_search_exact_no_match_returns_empty() {
        let trie = build_test_trie(concat!("t", line!()));
        let prefix: IpNet = "2.2.2.0/24".parse().unwrap();

        let results = trie.search(Some(prefix), None, None, None, None, true);
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_no_prefix_returns_all() {
        let trie = build_test_trie(concat!("t", line!()));

        let results = trie.search(None, None, None, None, None, true);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_validate() {
        let trie = build_test_trie(concat!("t", line!()));
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let ts = date_to_ts(date);

        let prefix: IpNet = "1.1.1.0/24".parse().unwrap();
        assert_eq!(trie.validate(&prefix, 13335, ts), RpkiValidation::Valid);
        assert_eq!(trie.validate(&prefix, 64512, ts), RpkiValidation::Invalid);

        let unknown: IpNet = "9.9.9.0/24".parse().unwrap();
        assert_eq!(trie.validate(&unknown, 13335, ts), RpkiValidation::Unknown);
    }

    #[test]
    fn test_lookup_prefix() {
        let trie = build_test_trie(concat!("t", line!()));
        let prefix: IpNet = "1.1.1.0/24".parse().unwrap();
        let results = trie.lookup_prefix(&prefix);
        assert_eq!(results.len(), 3);
    }
}
