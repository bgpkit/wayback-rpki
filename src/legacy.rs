//! Legacy v1 archive import (bincode + ipnet-trie).
//!
//! Only compiled with the `legacy` feature. Used by the `convert` subcommand
//! for one-time migration of old `roas_trie.bin.gz` archives to the v2 format.

use crate::{RoaRecordMut, RoasTrieMut};
use anyhow::Result;
use bincode::{Decode, Encode};
use ipnet_trie::IpnetTrie;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;

#[derive(Debug, Clone, Eq, PartialEq, Encode, Decode)]
struct LegacyRoasTrieEntry {
    max_len: u8,
    origin: u32,
    dates: HashSet<i64>,
    dates_compressed: VecDeque<(i64, i64)>,
}

/// Load a v1 (bincode + ipnet-trie) archive into a mutable v2 trie builder.
pub fn load_legacy(path: &str) -> Result<RoasTrieMut> {
    let mut bytes = Vec::new();
    oneio::get_reader(path)?.read_to_end(&mut bytes)?;
    let mut trie: IpnetTrie<HashMap<(u8, u32), LegacyRoasTrieEntry>> = IpnetTrie::new();
    trie.import_from_reader(&mut bytes.as_slice())?;

    let mut out = RoasTrieMut::new();
    let mut latest: i64 = 0;
    for (prefix, map) in trie.iter() {
        for entry in map.values() {
            let mut rec = RoaRecordMut::new_legacy(entry.max_len, entry.origin);
            rec.dates.extend(entry.dates.iter().copied());
            rec.ranges.extend(entry.dates_compressed.iter().copied());
            if let Some(d) = entry.dates.iter().max() {
                latest = latest.max(*d);
            }
            if let Some((_, end)) = entry.dates_compressed.back() {
                latest = latest.max(*end);
            }
            out.insert_record(prefix, rec);
        }
    }
    out.set_latest_date(latest);
    Ok(out)
}
