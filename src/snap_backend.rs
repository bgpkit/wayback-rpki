//! Serve the archive from a published `bgpkit-snap` object.
//!
//! This backend answers the same questions as the rkyv trie backend: lookup with the same
//! filters, validation, counts, and the latest date. The difference is what it reads. A snap
//! object is immutable and addressed by content id, so there is nothing to update in place:
//! a refresh means opening the next object, which is what publishing already produces.
//!
//! Selection keeps the v2 rule exactly, because a query answer that changes with the storage
//! format is a regression: with a prefix and `exact` the answer is that prefix alone, without
//! `exact` it is the subtree of the shortest covering prefix, and with no prefix it is the
//! whole table. Values are read out of the archived bytes; nothing is decoded into owned
//! records, so a query costs what the matching prefixes cost, not what the object costs.

use anyhow::{Context, Result};
use bgpkit_snap::Snapshot;
use chrono::NaiveDate;
use ipnet::IpNet;

use crate::roas_trie::ArchivedRoaRecord;
use crate::roas_trie::{
    date_to_ts, prefix_covers, ts_to_date, RoaRecord, RoasLookupEntry, RpkiValidation,
};
use crate::snap_export::{ArchivedRoaSet, RoaSet};

/// A snapshot opened for serving, plus the last day it covers.
pub struct SnapBackend {
    snap: Snapshot<RoaSet>,
    latest_ts: i64,
}

impl SnapBackend {
    /// Open a published object.
    pub fn open(path: &str) -> Result<Self> {
        let snap: Snapshot<RoaSet> =
            Snapshot::open(path).with_context(|| format!("opening {path}"))?;
        Self::from_snapshot(snap)
    }

    /// The same, from bytes already in memory.
    pub fn from_bytes(raw: &[u8]) -> Result<Self> {
        let snap: Snapshot<RoaSet> =
            Snapshot::from_bytes(raw).context("opening the object from bytes")?;
        Self::from_snapshot(snap)
    }

    fn from_snapshot(snap: Snapshot<RoaSet>) -> Result<Self> {
        // the window is what `current=true` means, so an object without one cannot answer
        // that query and must say so instead of guessing a date
        let (_, last) = snap
            .header()
            .window()
            .context("the object declares no coverage window")?;
        Ok(Self {
            snap,
            latest_ts: last,
        })
    }

    /// How many prefixes the object holds.
    pub fn len(&self) -> usize {
        self.snap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Prefix count per address family.
    pub fn counts(&self) -> (u64, u64) {
        let v4 = self.snap.v4().map(|m| m.len() as u64).unwrap_or(0);
        let v6 = self.snap.v6().map(|m| m.len() as u64).unwrap_or(0);
        (v4, v6)
    }

    /// The last day the object covers.
    pub fn latest_date_ts(&self) -> i64 {
        self.latest_ts
    }

    pub fn content_id(&self) -> String {
        self.snap.content_id()
    }

    /// All ROA records matching a prefix, including supernets and subnets.
    pub fn lookup_prefix(&self, prefix: &IpNet) -> Vec<RoasLookupEntry> {
        let mut entries = Vec::new();
        for (p, set) in self.subtree(prefix) {
            for record in set.0.iter() {
                entries.push(lookup_entry(p, record));
            }
        }
        entries
    }

    /// Search ROAs with optional filters, using the v2 selection rule.
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
            Some(true) => Some(self.latest_ts),
            Some(false) => {
                only_expired = true;
                None
            }
            None => date.map(date_to_ts),
        };
        let latest = self.latest_ts;

        let mut entries = Vec::new();
        for (p, records) in self.select(prefix, exact) {
            // deterministic order within a prefix, the same order the v2 backend returns
            let mut sorted: Vec<&ArchivedRoaRecord> = records.0.iter().collect();
            sorted.sort_by_key(|r| (r.origin.to_native(), r.max_len));
            for record in sorted {
                if origin.is_some_and(|o| record.origin.to_native() != o) {
                    continue;
                }
                if max_len.is_some_and(|m| record.max_len != m) {
                    continue;
                }
                if let Some(date_ts) = date_ts {
                    if !contains_date(record, date_ts) {
                        continue;
                    }
                }
                if only_expired && record.dates.iter().any(|t| t.1.to_native() >= latest) {
                    continue;
                }
                entries.push(lookup_entry(p, record));
            }
        }
        entries
    }

    /// RPKI validity of an announcement, by the v2 rule: a covering record with the same
    /// origin and a max_len that admits the prefix makes it valid, any other covering record
    /// that was in effect makes it invalid, and no covering record makes it unknown.
    pub fn validate(&self, prefix: &IpNet, origin: u32, date_ts: i64) -> RpkiValidation {
        let mut covered = false;
        for (record_prefix, records) in self.subtree(prefix) {
            if !prefix_covers(&record_prefix, prefix) {
                continue;
            }
            for record in records.0.iter() {
                if !contains_date(record, date_ts) {
                    continue;
                }
                covered = true;
                if record.origin.to_native() == origin && record.max_len >= prefix.prefix_len() {
                    return RpkiValidation::Valid;
                }
            }
        }
        if covered {
            RpkiValidation::Invalid
        } else {
            RpkiValidation::Unknown
        }
    }

    /// The v2 selection rule: exact, the subtree of the shortest covering prefix, or all.
    fn select(&self, prefix: Option<IpNet>, exact: bool) -> Vec<(IpNet, &ArchivedRoaSet)> {
        match prefix {
            Some(p) if exact => match self.snap.get_exact(p) {
                Some(set) => vec![(p, set)],
                None => Vec::new(),
            },
            Some(p) => self.subtree(&p),
            None => self.snap.iter().collect(),
        }
    }

    /// The shortest prefix at or above `prefix`, and everything below it.
    fn subtree(&self, prefix: &IpNet) -> Vec<(IpNet, &ArchivedRoaSet)> {
        let Some((spm, _)) = self.snap.spm(*prefix) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (p, set) in self.snap.iter_from(spm, true) {
            // iter_from walks the whole table in address order, so the subtree ends at the
            // first key that is no longer inside `spm`
            if !within(&spm, &p) {
                break;
            }
            out.push((p, set));
        }
        out
    }
}

/// True when `candidate` is `spm` or sits inside it.
fn within(spm: &IpNet, candidate: &IpNet) -> bool {
    match (spm, candidate) {
        (IpNet::V4(spm), IpNet::V4(c)) => spm.contains(&c.network()),
        (IpNet::V6(spm), IpNet::V6(c)) => spm.contains(&c.network()),
        _ => false,
    }
}

fn contains_date(record: &ArchivedRoaRecord, date_ts: i64) -> bool {
    record
        .dates
        .iter()
        .any(|t| date_ts >= t.0.to_native() && date_ts <= t.1.to_native())
}

fn lookup_entry(prefix: IpNet, record: &ArchivedRoaRecord) -> RoasLookupEntry {
    RoasLookupEntry {
        prefix,
        origin: record.origin.to_native(),
        max_len: record.max_len,
        dates_ranges: record
            .dates
            .iter()
            .map(|t| (ts_to_date(t.0.to_native()), ts_to_date(t.1.to_native())))
            .collect(),
    }
}

/// The record shape this backend expects, exposed so a caller can see what a published
/// object means without reading the trie code.
pub fn records_of(set: &ArchivedRoaSet) -> Vec<RoaRecord> {
    set.0
        .iter()
        .map(|record| RoaRecord {
            max_len: record.max_len,
            origin: record.origin.to_native(),
            dates: record
                .dates
                .iter()
                .map(|t| (t.0.to_native(), t.1.to_native()))
                .collect(),
        })
        .collect()
}
