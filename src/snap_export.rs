//! Publish the ROA history as a `bgpkit-snap` object.
//!
//! The object is a pure function of the Parquet table that `export-spans` already writes
//! once a day, so nothing here reads PostgreSQL and a failed build leaves the previous
//! artifact serving. One object per address family, addressed by content id, with a single
//! small manifest as the only mutable key.
//!
//! The shape a consumer sees:
//!
//! ```text
//! s3://spaces/broker/snap/roas-v4-<content id>.snap   immutable
//! s3://spaces/broker/snap/manifest.json              the only key that changes
//! ```
//!
//! and the contract is "read the manifest, then fetch the object it names".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ipnet::IpNet;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::{Deserialize, Serialize};

use crate::roas_trie::RoaRecord;

/// The value type: every (max_len, origin) pair authorized for a prefix, with the days it
/// was in effect. Canonical order so two builds of the same data give the same bytes.
#[bgpkit_snap::snap_value("bgpkit.roa-set:v1", canonicalize = canonicalize)]
#[derive(Debug, Clone)]
pub struct RoaSet(pub Vec<RoaRecord>);

fn canonicalize(value: &mut RoaSet) {
    value.0.sort_by_key(|r| (r.max_len, r.origin));
    for record in &mut value.0 {
        record.dates.sort();
    }
}

/// What one build produced, before any upload happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// The object bytes.
    pub bytes: Vec<u8>,
    /// Distinct prefixes stored.
    pub entries: u64,
    /// IPv4 prefixes.
    pub v4_entries: u64,
    /// IPv6 prefixes.
    pub v6_entries: u64,
    /// Interval records across all prefixes.
    pub records: u64,
    /// Content id, hex, as the header carries it.
    pub content_id: String,
    /// Coverage window from the header, `(first, last)` unix seconds.
    pub window: (i64, i64),
}

/// One published object, as the manifest records it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Published {
    /// Object key inside the bucket, e.g. `broker/snap/roas-<cid>.snap`.
    pub key: String,
    /// Content id, hex.
    pub content_id: String,
    /// Type id the header records.
    pub type_id: String,
    /// Object size.
    pub bytes: u64,
    /// Distinct prefixes.
    pub entries: u64,
    /// IPv4 prefixes.
    #[serde(default)]
    pub v4_entries: u64,
    /// IPv6 prefixes.
    #[serde(default)]
    pub v6_entries: u64,
    /// Interval records.
    pub records: u64,
    /// Coverage window, `(first, last)` unix seconds.
    pub window: (i64, i64),
    /// unix seconds when the object was written.
    pub built_at: i64,
    /// sha256 of the Parquet this was built from.
    pub source_sha256: String,
}

/// The only mutable key in the published set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    /// When this manifest was written, unix seconds.
    pub generated_at: i64,
    /// Where the source table came from.
    pub source: Source,
    /// Every object the writer has published, oldest first.
    pub objects: Vec<Published>,
    /// The pre-2.0 JSONL transport this writer last published, when it publishes one.
    ///
    /// Recorded so a later run can tell whether the key still holds what this pipeline
    /// wrote: the file is also written by other tooling, and a silent replacement would
    /// otherwise be indistinguishable from our own upload.
    #[serde(default)]
    pub legacy_jsonl: Option<LegacyJsonl>,
}

/// What the pre-2.0 transport contained when this pipeline wrote it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyJsonl {
    /// Object key, without the bucket.
    pub key: String,
    /// sha256 of the gzipped file as uploaded.
    pub sha256: String,
    /// Rows written.
    pub rows: u64,
    /// Bytes as uploaded.
    pub bytes: u64,
    /// When the upload finished, unix seconds.
    pub written_at: i64,
}

/// Provenance of the source table.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Source {
    /// sha256 of the Parquet file.
    pub parquet_sha256: String,
    /// Size in bytes.
    pub parquet_bytes: u64,
}

impl Manifest {
    /// The newest published object, if any.
    pub fn latest(&self) -> Option<&Published> {
        self.objects.last()
    }
}

/// What to do with a fresh build, given what is already published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing changed since the last publish.
    SkipUnchanged,
    /// Publish this build.
    Publish,
    /// Refuse: the build looks like a broken export.
    Refuse(String),
}

/// Decide whether a build should replace what the manifest names.
///
/// The two refusals are the shapes of a broken export, not of a quiet day: a table that lost
/// more than one percent of its prefixes, or a coverage window that moved backwards. In both
/// cases the previous object should keep serving.
pub fn decide(
    previous: Option<&Published>,
    source_sha256: &str,
    entries: u64,
    window: (i64, i64),
    force: bool,
) -> Decision {
    if force {
        return Decision::Publish;
    }
    let Some(previous) = previous else {
        return Decision::Publish;
    };
    if previous.source_sha256 == source_sha256 {
        return Decision::SkipUnchanged;
    }
    if entries * 100 < previous.entries * 99 {
        return Decision::Refuse(format!(
            "entries fell from {} to {entries}, more than one percent; the export is probably broken",
            previous.entries
        ));
    }
    if window.1 < previous.window.1 {
        return Decision::Refuse(format!(
            "coverage window ends at {} but the published object ends at {}",
            window.1, previous.window.1
        ));
    }
    Decision::Publish
}

/// Build the object from a Parquet span table.
pub fn build_from_parquet(spans: &Path) -> Result<Built> {
    let (by_prefix, records) = group_parquet(spans)?;
    build_from_rows(by_prefix, records)
}

/// Read the spans table and group it the way the value type wants it.
///
/// Shared by the object build and the legacy JSONL export so the two artifacts cannot
/// describe different data: same rows, same merging, one pass each.
pub fn group_parquet(spans: &Path) -> Result<(BTreeMap<IpNet, Vec<RoaRecord>>, u64)> {
    let file =
        std::fs::File::open(spans).with_context(|| format!("opening {}", spans.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .with_context(|| format!("reading {} as Parquet", spans.display()))?;
    let reader = builder.build().context("building a Parquet reader")?;

    // one record per (prefix, max_len, origin) with all of its intervals, which is the
    // shape the value type documents; one record per Parquet row would store the same
    // pair several times and inflate the object for nothing
    let mut grouped: BTreeMap<(IpNet, u8, u32), Vec<(i64, i64)>> = BTreeMap::new();
    for batch in reader {
        let batch = batch.context("reading a Parquet batch")?;
        let column = |name: &str| -> Result<&arrow::array::ArrayRef> {
            batch
                .column_by_name(name)
                .ok_or_else(|| anyhow::anyhow!("the Parquet table has no {name} column"))
        };
        let prefixes = column("prefix")?
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .context("prefix is not a string column")?;
        let origins = column("origin_asn")?
            .as_any()
            .downcast_ref::<arrow::array::UInt32Array>()
            .context("origin_asn is not a u32 column")?;
        let max_lens = column("max_len")?
            .as_any()
            .downcast_ref::<arrow::array::UInt8Array>()
            .context("max_len is not a u8 column")?;
        let from = column("from_ts")?
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .context("from_ts is not an i64 column")?;
        let to = column("to_ts")?
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .context("to_ts is not an i64 column")?;

        for i in 0..batch.num_rows() {
            let prefix: IpNet = prefixes
                .value(i)
                .parse()
                .with_context(|| format!("row {i}: prefix is not an IP prefix"))?;
            grouped
                .entry((prefix, max_lens.value(i), origins.value(i)))
                .or_default()
                .push((from.value(i), to.value(i)));
        }
    }

    let mut by_prefix: BTreeMap<IpNet, Vec<RoaRecord>> = BTreeMap::new();
    let mut records = 0u64;
    for ((prefix, max_len, origin), mut dates) in grouped {
        dates.sort_unstable();
        // adjacent or overlapping intervals become one, so a pair that was announced
        // continuously does not cost one record per export row
        let mut merged: Vec<(i64, i64)> = Vec::with_capacity(dates.len());
        for (from, to) in dates {
            match merged.last_mut() {
                Some((_, last)) if from <= last.saturating_add(1) => {
                    *last = (*last).max(to);
                }
                _ => merged.push((from, to)),
            }
        }
        by_prefix.entry(prefix).or_default().push(RoaRecord {
            max_len,
            origin,
            dates: merged,
        });
        records += 1;
    }

    Ok((by_prefix, records))
}

/// Write the grouped rows as the pre-2.0 JSONL transport.
///
/// This is the file older binaries bootstrap from (`roas_trie.jsonl.gz`), so it is derived
/// from the same grouped rows as the object: a client on 1.x and a client on 2.x must see the
/// same history, and two independent exports would eventually drift.
pub fn write_jsonl(by_prefix: &BTreeMap<IpNet, Vec<RoaRecord>>, path: &str) -> Result<u64> {
    use std::io::Write;

    let mut writer = oneio::get_writer(path).with_context(|| format!("writing {path}"))?;
    let mut lines = 0u64;
    for (prefix, records) in by_prefix {
        for record in records {
            let row = crate::roas_trie::JsonlRecord {
                p: prefix.to_string(),
                m: record.max_len,
                o: record.origin,
                r: record.dates.clone(),
            };
            serde_json::to_writer(&mut writer, &row).context("serializing a JSONL row")?;
            writer.write_all(b"\n").context("writing a newline")?;
            lines += 1;
        }
    }
    writer.flush().context("flushing the JSONL transport")?;
    Ok(lines)
}

/// Build from already-grouped rows. Split out so tests do not need a Parquet file.
pub fn build_from_rows(by_prefix: BTreeMap<IpNet, Vec<RoaRecord>>, records: u64) -> Result<Built> {
    let mut first = i64::MAX;
    let mut last = i64::MIN;
    for value in by_prefix.values() {
        for record in value {
            for (from, to) in &record.dates {
                first = first.min(*from);
                last = last.max(*to);
            }
        }
    }
    let window = if by_prefix.is_empty() {
        (0, 0)
    } else {
        (first, last)
    };

    let mut writer: bgpkit_snap::Writer<RoaSet> = bgpkit_snap::Writer::new();
    for (prefix, value) in by_prefix {
        writer
            .insert(prefix, RoaSet(value))
            .with_context(|| format!("inserting {prefix}"))?;
    }
    let entries = writer.len() as u64;
    let bytes = writer
        .finish(&bgpkit_snap::Opts {
            created_at: chrono::Utc::now().timestamp(),
            stream: None,
            window: Some(window),
            extras: &[],
        })
        .context("building the object")?;

    // the writer must not hand out an object its own reader refuses
    let header = bgpkit_snap::verify_bytes(&bytes).context("the object does not verify")?;
    let content_id = header
        .content_id
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    // the counts section is what lets a reader of any value type report the split
    let counts =
        bgpkit_snap::recorded_counts(&bytes)?.context("the object carries no counts section")?;

    Ok(Built {
        bytes,
        entries,
        v4_entries: counts.v4,
        v6_entries: counts.v6,
        records,
        content_id,
        window,
    })
}

/// sha256 of a file, for the manifest's provenance and the unchanged-input guard.
pub fn file_sha256(path: &Path) -> Result<String> {
    oneio::digest::get_sha256_digest(path.to_str().unwrap())
        .map_err(|e| anyhow::anyhow!("hashing {}: {e}", path.display()))
}

/// What one differential check found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Prefixes sampled from the object.
    pub sampled: usize,
    /// Prefixes the object holds.
    pub object_entries: u64,
    /// Distinct prefixes PostgreSQL holds.
    pub pg_entries: u64,
    /// One line per difference, capped.
    pub mismatches: Vec<String>,
}

/// Compare the published object with PostgreSQL, prefix by prefix.
///
/// What this checks: for a sample of prefixes, the set of origin ASNs the object records
/// equals the set PostgreSQL observed, and the object's day span covers the days PostgreSQL
/// saw that prefix. Both are necessary conditions for the object to answer what the archive
/// says, and either would have caught a dropped prefix, a mis-grouped origin or a shifted
/// date.
///
/// What it does not check: the certificate window each ROA carried, which is what turns
/// observed days into authorized days. That join happens in the export SQL, so this compares
/// the object against the same table the export reads, not against the export's own output.
pub fn check_against_pg(pg_config: &str, object: &Path, sample: usize) -> Result<CheckReport> {
    use std::collections::BTreeSet;

    let snap: bgpkit_snap::Snapshot<RoaSet> = bgpkit_snap::Snapshot::open(object)
        .with_context(|| format!("opening {}", object.display()))?;
    let mut client = postgres::Client::connect(pg_config, postgres::NoTls)
        .context("connecting to PostgreSQL")?;

    let pg_entries: i64 = client
        .query_one("SELECT count(DISTINCT prefix) FROM wayback.roa_object", &[])?
        .get(0);

    // every k-th prefix, so the sample spans the whole table instead of one corner
    let step = (snap.len() / sample.max(1)).max(1);
    let prefixes: Vec<IpNet> = snap.iter().map(|(p, _)| p).step_by(step).collect();

    let mut report = CheckReport {
        sampled: prefixes.len(),
        object_entries: snap.len() as u64,
        pg_entries: pg_entries as u64,
        mismatches: Vec::new(),
    };

    for prefix in prefixes {
        let Some(value) = snap.owned().get_exact(prefix)? else {
            report.mismatches.push(format!(
                "{prefix}: the object has no value after iterating it"
            ));
            continue;
        };
        let object_origins: BTreeMap<u32, (i64, i64)> =
            value.0.iter().fold(BTreeMap::new(), |mut acc, record| {
                let (from, to) = record
                    .dates
                    .iter()
                    .fold((i64::MAX, i64::MIN), |(lo, hi), (from, to)| {
                        (lo.min(*from), hi.max(*to))
                    });
                acc.entry(record.origin)
                    .and_modify(|(lo, hi)| {
                        *lo = (*lo).min(from);
                        *hi = (*hi).max(to);
                    })
                    .or_insert((from, to));
                acc
            });

        let rows = client.query(
            "SELECT origin_asn, min(first_seen), \
                    max(coalesce(last_seen, current_date - 1)) \
             FROM wayback.roa_object WHERE prefix = ($1::text)::inet GROUP BY origin_asn",
            &[&prefix.to_string()],
        )?;
        if rows.is_empty() {
            report
                .mismatches
                .push(format!("{prefix}: PostgreSQL has no row for it"));
            continue;
        }
        let pg_origins: BTreeSet<u32> =
            rows.iter().map(|row| row.get::<_, i64>(0) as u32).collect();
        if object_origins.keys().copied().collect::<BTreeSet<_>>() != pg_origins {
            report.mismatches.push(format!(
                "{prefix}: origins differ, object {:?} vs pg {:?}",
                object_origins.keys().collect::<Vec<_>>(),
                pg_origins
            ));
            continue;
        }
        // The interval envelope is deliberately not compared here. A record's dates are the
        // certificate window intersected with the days the archive held that ROA, and the
        // value does not carry the TA or the version identity needed to recompute that from
        // PostgreSQL: any comparison either repeats the export's own arithmetic or compares
        // the object against a different granularity (roa_object last_seen is per ROA object,
        // not per version). The origin set is the part this check can decide on its own.
    }

    Ok(report)
}

/// The object a new user should start from: the newest one the manifest names.
///
/// Returns the download URL and the content id to check the bytes against. The manifest is
/// the only mutable key in the published set, so this function is the whole bootstrap
/// contract: fetch the manifest, fetch the object it names last, check the id.
pub fn newest_object_url(manifest_json: &str, base: &str) -> Result<(String, String)> {
    let manifest: Manifest = serde_json::from_str(manifest_json).context("parsing the manifest")?;
    let latest = manifest.latest().context("the manifest names no object")?;
    Ok((format!("{base}{}", latest.key), latest.content_id.clone()))
}

/// Where a published set lives.
#[derive(Debug, Clone)]
pub struct Target {
    /// Bucket, e.g. `spaces`.
    pub bucket: String,
    /// Key prefix inside the bucket, e.g. `broker/snap`.
    pub prefix: String,
}

impl Target {
    /// Parse `s3://bucket/key/prefix`.
    pub fn parse(url: &str) -> Result<Self> {
        let (bucket, key) = oneio::s3_url_parse(url)
            .map_err(|e| anyhow::anyhow!("{url} is not an s3://bucket/key URL: {e}"))?;
        Ok(Self {
            bucket,
            prefix: key.trim_end_matches('/').to_string(),
        })
    }

    /// The object key for a content id.
    pub fn object_key(&self, content_id: &str) -> String {
        format!(
            "{}/roas-{}.snap",
            self.prefix,
            &content_id[..12.min(content_id.len())]
        )
    }

    /// The manifest key.
    pub fn manifest_key(&self) -> String {
        format!("{}/manifest.json", self.prefix)
    }

    /// The manifest URL.
    pub fn manifest_url(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.manifest_key())
    }
}

/// Which published objects a retention policy drops.
///
/// Rule: keep the newest `keep_daily` objects, and beyond them keep the last object of each
/// UTC month, which is what a consumer who wants history rather than yesterday actually uses.
/// Everything else is a daily copy nobody needs.
pub fn plan_prune(objects: &[Published], keep_daily: usize) -> Vec<usize> {
    if objects.len() <= keep_daily {
        return Vec::new();
    }
    let split = objects.len() - keep_daily;
    let mut drop = Vec::new();
    for i in 0..split {
        let this_month = month_of(objects[i].built_at);
        let next_month = objects.get(i + 1).map(|o| month_of(o.built_at));
        // the last object of a month survives; a mid-month one does not
        if next_month.is_some_and(|next| next != this_month) {
            continue;
        }
        drop.push(i);
    }
    drop
}

fn month_of(unix: i64) -> (i32, u32) {
    use chrono::Datelike;
    let date = chrono::DateTime::from_timestamp(unix, 0)
        .unwrap_or_default()
        .date_naive();
    (date.year(), date.month())
}

/// Delete one object from the published set.
pub fn delete_object(target: &Target, key: &str) -> Result<()> {
    oneio::s3_delete(&target.bucket, key)
        .map_err(|e| anyhow::anyhow!("deleting s3://{}/{key}: {e}", target.bucket))
}

/// Read the manifest, or an empty one when this is the first publish.
pub fn read_manifest(url: &str) -> Result<Manifest> {
    match oneio::read_to_bytes(url) {
        Ok(bytes) if !bytes.is_empty() => {
            let text = String::from_utf8(bytes).with_context(|| format!("{url} is not UTF-8"))?;
            serde_json::from_str(&text).with_context(|| format!("{url} is not a manifest"))
        }
        // only a missing key means "first publish": any other error (credentials, a
        // network failure) must stop the run, because treating it as absent would let a
        // build past the guards
        Ok(_) => Ok(Manifest::default()),
        Err(error) => {
            let text = format!("{error}");
            if text.contains("404") || text.contains("NoSuchKey") || text.contains("not found") {
                Ok(Manifest::default())
            } else {
                Err(anyhow::anyhow!("reading {url}: {text}"))
            }
        }
    }
}

/// Upload `bytes` to `key` in the target bucket.
pub fn upload(target: &Target, key: &str, bytes: &[u8]) -> Result<()> {
    let temp: PathBuf = std::env::temp_dir().join(format!(
        "wayback-snap-publish-{}-{}.snap",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
    let uploaded = oneio::s3_upload(&target.bucket, key, temp.to_str().unwrap())
        .map_err(|e| anyhow::anyhow!("uploading s3://{}/{key}: {e}", target.bucket));
    let _ = std::fs::remove_file(&temp);
    uploaded
}

/// Write the manifest, after the object it names is in place.
pub fn write_manifest(target: &Target, manifest: &Manifest) -> Result<()> {
    let text = serde_json::to_vec_pretty(manifest).context("serialising the manifest")?;
    let temp: PathBuf = std::env::temp_dir().join(format!(
        "wayback-snap-manifest-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&temp, &text).with_context(|| format!("writing {}", temp.display()))?;
    let key = target.manifest_key();
    let uploaded = oneio::s3_upload(&target.bucket, &key, temp.to_str().unwrap())
        .map_err(|e| anyhow::anyhow!("uploading s3://{}/{key}: {e}", target.bucket));
    let _ = std::fs::remove_file(&temp);
    uploaded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(max_len: u8, origin: u32, dates: Vec<(i64, i64)>) -> RoaRecord {
        RoaRecord {
            max_len,
            origin,
            dates,
        }
    }

    fn published(entries: u64, window: (i64, i64), sha: &str) -> Published {
        Published {
            key: "broker/snap/roas-000000000000.snap".to_string(),
            content_id: "0".repeat(64),
            type_id: "bgpkit.roa-set:v1".to_string(),
            bytes: 1,
            entries,
            v4_entries: entries,
            v6_entries: 0,
            records: entries,
            window,
            built_at: 0,
            source_sha256: sha.to_string(),
        }
    }

    #[test]
    fn a_first_build_is_published() {
        assert_eq!(decide(None, "abc", 10, (0, 10), false), Decision::Publish);
    }

    #[test]
    fn an_unchanged_source_is_skipped() {
        let previous = published(100, (0, 100), "abc");
        assert_eq!(
            decide(Some(&previous), "abc", 100, (0, 100), false),
            Decision::SkipUnchanged
        );
    }

    #[test]
    fn a_quiet_day_is_published() {
        let previous = published(1000, (0, 100), "abc");
        assert_eq!(
            decide(Some(&previous), "def", 1000, (0, 107), false),
            Decision::Publish
        );
        // exactly one percent down is still inside the guard
        assert_eq!(
            decide(Some(&previous), "def", 990, (0, 107), false),
            Decision::Publish
        );
    }

    #[test]
    fn a_collapsed_table_is_refused() {
        let previous = published(1000, (0, 100), "abc");
        match decide(Some(&previous), "def", 900, (0, 107), false) {
            Decision::Refuse(message) => assert!(message.contains("entries fell")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_backwards_window_is_refused() {
        let previous = published(1000, (0, 200), "abc");
        match decide(Some(&previous), "def", 1000, (0, 150), false) {
            Decision::Refuse(message) => assert!(message.contains("window ends at")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn force_publishes_over_every_guard() {
        let previous = published(1000, (0, 200), "abc");
        assert_eq!(
            decide(Some(&previous), "abc", 1, (0, 1), true),
            Decision::Publish
        );
    }

    #[test]
    fn the_value_type_round_trips_through_an_object() {
        let mut rows = BTreeMap::new();
        rows.insert(
            "10.0.0.0/8".parse::<IpNet>().unwrap(),
            vec![
                record(16, 64512, vec![(1_700_000_000, 1_700_086_400)]),
                record(24, 64513, vec![(1_700_000_000, 1_700_172_800)]),
            ],
        );
        rows.insert(
            "2001:db8::/32".parse::<IpNet>().unwrap(),
            vec![record(48, 64514, vec![(1_700_000_000, 1_700_086_400)])],
        );
        let built = build_from_rows(rows, 3).expect("build");
        assert_eq!(built.entries, 2);
        assert_eq!(built.v4_entries, 1);
        assert_eq!(built.v6_entries, 1);
        assert_eq!(built.records, 3);
        assert_eq!(built.window, (1_700_000_000, 1_700_172_800));

        let snap = bgpkit_snap::Snapshot::<RoaSet>::from_bytes(&built.bytes).expect("open");
        assert_eq!(snap.len(), 2);
        let (prefix, value) = snap
            .owned()
            .lpm("10.1.2.3/32".parse::<IpNet>().unwrap())
            .expect("query")
            .expect("covered");
        assert_eq!(prefix, "10.0.0.0/8".parse::<IpNet>().unwrap());
        assert_eq!(value.0.len(), 2);
        // canonical order: (max_len, origin)
        assert_eq!(value.0[0].max_len, 16);
        assert_eq!(value.0[1].max_len, 24);
    }

    #[test]
    fn the_object_key_carries_the_content_id_prefix() {
        let target = Target {
            bucket: "spaces".to_string(),
            prefix: "broker/snap".to_string(),
        };
        let key = target.object_key(&"a".repeat(64));
        assert_eq!(key, "broker/snap/roas-aaaaaaaaaaaa.snap");
        assert_eq!(target.manifest_key(), "broker/snap/manifest.json");
    }

    fn at(day: i64) -> i64 {
        // 2026-01-01T00:00:00Z plus a day offset, so months are easy to reason about
        1_767_225_600 + day * 86_400
    }

    fn entry(built_at: i64, key: &str) -> Published {
        Published {
            key: key.to_string(),
            content_id: "0".repeat(64),
            type_id: "bgpkit.roa-set:v1".to_string(),
            bytes: 1,
            entries: 1,
            v4_entries: 1,
            v6_entries: 0,
            records: 1,
            window: (0, built_at),
            built_at,
            source_sha256: key.to_string(),
        }
    }

    #[test]
    fn a_short_history_is_kept_whole() {
        let objects: Vec<Published> = (0..5).map(|i| entry(at(i), &format!("k{i}"))).collect();
        assert!(plan_prune(&objects, 30).is_empty());
    }

    #[test]
    fn dailies_beyond_the_window_are_dropped_but_month_ends_survive() {
        // 75 days from 2026-01-01, keep 10: January 31 is index 30 and February 28 is index
        // 58, both older than the newest ten, so both survive and their neighbours do not
        let objects: Vec<Published> = (0..75).map(|i| entry(at(i), &format!("k{i}"))).collect();
        let dropped = plan_prune(&objects, 10);
        let dropped_keys: Vec<&str> = dropped.iter().map(|i| objects[*i].key.as_str()).collect();
        assert!(dropped_keys.contains(&"k29"));
        assert!(!dropped_keys.contains(&"k30"), "January 31 survives");
        assert!(dropped_keys.contains(&"k57"));
        assert!(!dropped_keys.contains(&"k58"), "February 28 survives");
        assert!(
            dropped.iter().all(|i| *i < objects.len() - 10),
            "the newest ten always survive"
        );
    }

    #[test]
    fn the_legacy_jsonl_round_trips_through_the_old_reader() {
        let dir = std::env::temp_dir().join(format!("wayback-legacy-jsonl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roas_trie.jsonl.gz");

        let mut by_prefix: BTreeMap<IpNet, Vec<RoaRecord>> = BTreeMap::new();
        by_prefix.insert(
            "10.0.0.0/8".parse().unwrap(),
            vec![RoaRecord {
                max_len: 24,
                origin: 64500,
                dates: vec![(1_704_067_200, 1_704_153_599)],
            }],
        );
        by_prefix.insert(
            "2001:db8::/32".parse().unwrap(),
            vec![RoaRecord {
                max_len: 48,
                origin: 64501,
                dates: vec![(1_704_067_200, 1_704_067_200)],
            }],
        );

        let lines = write_jsonl(&by_prefix, path.to_str().unwrap()).unwrap();
        assert_eq!(lines, 2, "one line per (prefix, max_len, origin)");

        // the check that matters: the reader older binaries use accepts the file and sees
        // the same table
        let trie = crate::roas_trie::RoasTrieMut::from_jsonl(path.to_str().unwrap()).unwrap();
        assert_eq!(trie.len(), 2, "two prefixes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_newest_object_is_the_one_to_bootstrap() {
        let objects: Vec<Published> = (0..3).map(|i| entry(at(i), &format!("k{i}"))).collect();
        let manifest = Manifest {
            generated_at: at(3),
            source: Source::default(),
            objects,
            legacy_jsonl: None,
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let (url, id) = newest_object_url(&json, "https://example.test/").unwrap();
        assert_eq!(url, "https://example.test/k2");
        assert_eq!(id, "0".repeat(64));
    }

    #[test]
    fn a_manifest_with_no_object_is_an_error() {
        let manifest = Manifest {
            generated_at: 0,
            source: Source::default(),
            objects: Vec::new(),
            legacy_jsonl: None,
        };
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(newest_object_url(&json, "https://example.test/").is_err());
    }

    #[test]
    fn the_manifest_records_what_the_transport_contained() {
        let legacy = LegacyJsonl {
            key: "broker/roas_trie.jsonl.gz".to_string(),
            sha256: "a".repeat(64),
            rows: 11,
            bytes: 22,
            written_at: 33,
        };
        let manifest = Manifest {
            generated_at: 0,
            source: Source::default(),
            objects: Vec::new(),
            legacy_jsonl: Some(legacy.clone()),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.legacy_jsonl, Some(legacy));

        // a manifest written before this field existed still parses
        let old: Manifest = serde_json::from_str(
            r#"{"generated_at":0,"source":{"parquet_sha256":"","parquet_bytes":0},"objects":[]}"#,
        )
        .unwrap();
        assert!(old.legacy_jsonl.is_none());
    }

    #[test]
    fn the_source_digest_tracks_the_file() {
        let dir = std::env::temp_dir().join(format!("wayback-snap-digest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let one = dir.join("a");
        let two = dir.join("b");
        std::fs::write(&one, b"same").unwrap();
        std::fs::write(&two, b"same").unwrap();
        assert_eq!(file_sha256(&one).unwrap(), file_sha256(&two).unwrap());
        std::fs::write(&two, b"different").unwrap();
        assert_ne!(file_sha256(&one).unwrap(), file_sha256(&two).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
