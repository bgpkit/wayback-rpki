//! v2.1 PostgreSQL backfill (PoC): object-grain ROA history.
//!
//! Ingest contract (per (tal, day D), one transaction):
//! - new (ta, uri, prefix, origin)          -> INSERT object + first version row
//! - present, attributes unchanged          -> no write at all
//! - same URI, max_len or cert window moved -> close old version (last_seen=D-1),
//!   open new version (first_seen=D)
//! - in current set but absent from file    -> close the day's span (splitting
//!   it when later days were already ingested); the object marker follows the
//!   remaining spans
//! - absent file (HTTP failure / no file)   -> no disappearance decisions; ledger row only
//!
//! Idempotent: replaying a day is a no-op.

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use postgres::{Client, NoTls};
use std::collections::{BTreeMap, HashMap};

use crate::{try_crawl_tal_after, ROA_ARTIFACT};

/// Resolve the requested TAL names to archive URLs. `get_tal_urls` panics on an
/// unknown name, so every name is resolved through `tal_url` and reported as a
/// CLI error instead of aborting the process.
pub(crate) fn selected_tal_urls(tals: &[String]) -> Result<Vec<String>> {
    if tals.is_empty() {
        return Ok(crate::tal_names()
            .into_iter()
            .filter_map(crate::tal_url)
            .map(str::to_string)
            .collect());
    }
    let mut urls: Vec<String> = Vec::with_capacity(tals.len());
    for tal in tals {
        let Some(url) = crate::tal_url(tal) else {
            bail!(
                "unknown TAL {tal:?}; expected one of {}",
                crate::tal_names().join(", ")
            );
        };
        if !urls.iter().any(|known| known == url) {
            urls.push(url.to_string());
        }
    }
    Ok(urls)
}

/// One CSV row with full fidelity (v2.1): URI and certificate window kept.
#[derive(Debug, Clone, PartialEq)]
pub struct RoaFullEntry {
    pub uri: String,
    pub prefix: String,
    pub origin_asn: u32,
    pub max_len: u8,
    pub not_before: chrono::NaiveDateTime,
    pub not_after: chrono::NaiveDateTime,
}

/// Parse the Max Length field. RIPE uses an empty field to mean the ROA's
/// prefix length, but any nonempty malformed value is a source error.
fn parse_max_len(value: &str, prefix: &str, line: &str) -> Result<u8> {
    if !value.is_empty() {
        return value
            .parse()
            .with_context(|| format!("bad Max Length in row: {line}"));
    }

    prefix
        .rsplit_once('/')
        .map(|(_, length)| length)
        .context("empty Max Length with no prefix length")?
        .parse()
        .with_context(|| format!("bad prefix length in row: {line}"))
}

/// Parse a RIPE `roas.csv.xz` source URL or local CSV path, keeping URI, Not
/// Before, and Not After. Accepts both the quoted-URI legacy variant (pre-2018)
/// and the current unquoted one.
pub fn parse_roas_csv_full(path: &str) -> Result<Vec<RoaFullEntry>> {
    let mut out = Vec::new();
    let mut header_seen = false;
    for line in oneio::read_lines_lossy(path)? {
        let line = line.context("read ROA CSV line")?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !header_seen {
            if line.starts_with("URI") {
                header_seen = true;
            }
            continue;
        }
        // Fields: URI,ASN,IP Prefix,Max Length,Not Before,Not After
        // (legacy rows may quote the URI; timestamps are "YYYY-MM-DD HH:MM:SS")
        let fields: Vec<&str> = split_csv_row(line);
        if fields.len() < 6 {
            anyhow::bail!("malformed CSV row ({} fields): {}", fields.len(), line);
        }
        let uri = fields[0].trim_matches('"').to_string();
        let origin_asn: u32 = fields[1]
            .trim_start_matches("AS")
            .parse()
            .with_context(|| format!("bad ASN in row: {line}"))?;
        let not_before = chrono::NaiveDateTime::parse_from_str(fields[4], "%Y-%m-%d %H:%M:%S")
            .with_context(|| format!("bad Not Before in row: {line}"))?;
        let not_after = chrono::NaiveDateTime::parse_from_str(fields[5], "%Y-%m-%d %H:%M:%S")
            .with_context(|| format!("bad Not After in row: {line}"))?;
        out.push(RoaFullEntry {
            uri,
            prefix: fields[2].to_string(),
            origin_asn,
            max_len: parse_max_len(fields[3], fields[2], line)?,
            not_before,
            not_after,
        });
    }
    if !header_seen {
        bail!("ROA CSV header not found in {path}");
    }
    Ok(out)
}

/// Escape a field for PostgreSQL text-format COPY (tab, newline, backslash).
fn escape_tsv(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Minimal CSV splitter honoring double-quoted fields (legacy URI variant).
fn split_csv_row(line: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut in_quotes = false;
    let mut start = 0usize;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(&line[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    fields.push(&line[start..]);
    fields
}

/// In-memory state of one object's current version, used for diffing.
#[derive(Debug, Clone, PartialEq)]
struct CurrentVersion {
    roa_obj_id: i64,
    version_first_seen: NaiveDate,
    max_len: u8,
    not_before: chrono::NaiveDateTime,
    not_after: chrono::NaiveDateTime,
}

#[derive(Default)]
struct IngestCounts {
    objects_inserted: i64,
    versions_inserted: i64,
    versions_closed: i64,
    /// Spans extended because a repaired day continued an existing span.
    spans_extended: i64,
    /// Object markers rewritten. `roa_object.last_seen` is a denormalized
    /// current-state marker, recomputed from the version spans rather than
    /// written by the day being applied.
    object_markers_updated: i64,
}

/// Load all current objects (last_seen IS NULL) keyed by (ta, uri, prefix, origin).
fn load_current_objects(
    client: &mut postgres::Transaction,
    tal: &str,
    day: NaiveDate,
) -> Result<HashMap<(String, String, String, i64), CurrentVersion>> {
    let mut map = HashMap::new();
    // The version that was current ON `day`: latest version whose span covers
    // `day` (open-ended counts as covering). This makes day-D replay idempotent
    // even after later days were ingested.
    for row in client
        .query(
            "SELECT o.roa_obj_id, o.prefix::text, o.origin_asn, o.ta, o.uri,
                    v.max_len, v.not_before, v.not_after, v.first_seen
             FROM wayback.roa_object o
             JOIN wayback.roa_version v ON v.roa_obj_id = o.roa_obj_id
             WHERE o.ta = $2 AND v.first_seen <= $1 AND (v.last_seen IS NULL OR v.last_seen >= $1)",
            &[&day, &tal],
        )
        .context("load current objects")?
    {
        let roa_obj_id: i64 = row.get(0);
        let prefix: String = row.get(1);
        let origin_asn: i64 = row.get(2);
        let ta: String = row.get(3);
        let uri: String = row.get(4);
        let max_len: i16 = row.get(5);
        let not_before_utc: chrono::DateTime<chrono::Utc> = row.get(6);
        let not_after_utc: chrono::DateTime<chrono::Utc> = row.get(7);
        let not_before = not_before_utc.naive_utc();
        let not_after = not_after_utc.naive_utc();
        map.insert(
            (ta, uri, prefix, origin_asn),
            CurrentVersion {
                roa_obj_id,
                version_first_seen: row.get(8),
                max_len: max_len as u8,
                not_before,
                not_after,
            },
        );
    }
    Ok(map)
}

/// Return the final observation date when an active span is absent on `day`.
/// A span that starts on `day` cannot be closed at `day - 1` without creating
/// a reverse interval.
fn last_seen_before_absence(first_seen: NaiveDate, day: NaiveDate) -> Option<NaiveDate> {
    let previous_day = day - chrono::Duration::days(1);
    (first_seen <= previous_day).then_some(previous_day)
}

/// Stage the days after `day` that the span still covers, so a repair can keep
/// them as their own span. An open span only reaches as far as the days the TAL
/// actually observed: without an observed day after `day` there is no later part
/// to carry, and inventing one would claim coverage past the archive frontier.
fn stage_span_tail(
    tx: &mut postgres::Transaction<'_>,
    roa_obj_id: i64,
    version_first_seen: NaiveDate,
    day: NaiveDate,
) -> Result<()> {
    tx.execute(
        "INSERT INTO stage_split (roa_obj_id, max_len, not_before, not_after, first_seen, last_seen)
         SELECT v.roa_obj_id, v.max_len, v.not_before, v.not_after, $1::date + 1, v.last_seen
           FROM wayback.roa_version v
          WHERE v.roa_obj_id = $2 AND v.first_seen = $3
            AND COALESCE(v.last_seen, 'infinity'::date) > $1::date
            AND (v.last_seen IS NOT NULL
                 OR EXISTS (SELECT 1 FROM wayback.roa_object o
                              JOIN wayback.source_file f
                                ON f.tal = o.ta AND f.artifact = 'roas.csv.xz'
                             WHERE o.roa_obj_id = v.roa_obj_id
                               AND f.file_date >= $1::date + 1
                               AND f.gap_class = 'observed'))",
        &[&day, &roa_obj_id, &version_first_seen],
    )?;
    Ok(())
}

/// Trim the span observed on `day` back to `last_seen`, never inverting a range
/// and never touching a span that starts after it.
fn trim_span_to_day_before(
    tx: &mut postgres::Transaction<'_>,
    roa_obj_id: i64,
    version_first_seen: NaiveDate,
    last_seen: NaiveDate,
) -> Result<i64> {
    Ok(tx.execute(
        "UPDATE wayback.roa_version
            SET last_seen = $1
          WHERE roa_obj_id = $2 AND first_seen = $3
            AND COALESCE(last_seen, 'infinity'::date) > $1::date
            AND first_seen <= $1::date",
        &[&last_seen, &roa_obj_id, &version_first_seen],
    )? as i64)
}

/// Write the carried-over tail of a split span and clear the scratch list.
fn insert_staged_tail(tx: &mut postgres::Transaction<'_>) -> Result<i64> {
    let inserted = tx.execute(
        "INSERT INTO wayback.roa_version (roa_obj_id, max_len, not_before, not_after, first_seen, last_seen)
         SELECT roa_obj_id, max_len, not_before, not_after, first_seen, last_seen FROM stage_split",
        &[],
    )? as i64;
    tx.execute("TRUNCATE stage_split", &[])?;
    Ok(inserted)
}

/// Close the span that covers `day` on `last_seen`, keeping the days after
/// `day` as their own span. Trimming alone would drop coverage that later
/// ingests already observed, and closing only an open span would leave the
/// repaired day covered by a span the file says is not there.
fn close_span_with_split(
    tx: &mut postgres::Transaction<'_>,
    roa_obj_id: i64,
    version_first_seen: NaiveDate,
    day: NaiveDate,
    last_seen: NaiveDate,
    counts: &mut IngestCounts,
) -> Result<()> {
    stage_span_tail(tx, roa_obj_id, version_first_seen, day)?;
    counts.versions_closed +=
        trim_span_to_day_before(tx, roa_obj_id, version_first_seen, last_seen)?;
    counts.versions_inserted += insert_staged_tail(tx)?;
    Ok(())
}

/// Apply one day's entries for one TAL. `file_ok=false` records the gap and
/// makes no disappearance decisions.
#[allow(clippy::too_many_arguments)]
pub fn ingest_day(
    client: &mut Client,
    tal: &str,
    day: NaiveDate,
    entries: &[RoaFullEntry],
    file_ok: bool,
    http_status: Option<i16>,
    sha256: Option<&str>,
) -> Result<(i64, i64)> {
    let prev_day = day - chrono::Duration::days(1);

    let mut tx = client.transaction().context("begin transaction")?;

    // Ledger row first (idempotent upsert). A file that could not be read has no
    // count: NULL keeps roa_counts_view from reporting it as an observed zero.
    tx.execute(
        "INSERT INTO wayback.source_file (tal, file_date, artifact, http_status, roa_count, sha256, gap_class)
         VALUES ($1, $2, 'roas.csv.xz', $3, $4, $5, $6)
         ON CONFLICT (tal, file_date, artifact) DO UPDATE
           SET http_status = EXCLUDED.http_status,
               roa_count = EXCLUDED.roa_count,
               sha256 = EXCLUDED.sha256,
               gap_class = EXCLUDED.gap_class",
        &[
            &tal,
            &day,
            &http_status,
            &file_ok.then_some(entries.len() as i32),
            &sha256,
            &(if file_ok { "observed" } else { "missing" }),
        ],
    )?;

    // Normalize today's set keyed like the DB rows.
    let mut today: HashMap<(String, String, String, i64), &RoaFullEntry> = HashMap::new();
    for e in entries {
        // One (uri, prefix, origin) can carry several max_len rows in a single
        // day (publisher-side quirk); keep the smallest deterministically so
        // day-to-day comparisons are stable.
        let key = (
            tal.to_string(),
            e.uri.clone(),
            e.prefix.clone(),
            e.origin_asn as i64,
        );
        match today.get(&key) {
            Some(prev) if prev.max_len <= e.max_len => {}
            _ => {
                today.insert(key, e);
            }
        }
    }

    let current = load_current_objects(&mut tx, tal, day)?;
    let mut counts = IngestCounts::default();

    // Days after a repaired day that a span still covers are carried over here.
    tx.execute(
        "CREATE TEMP TABLE IF NOT EXISTS stage_split (
           roa_obj_id bigint, max_len smallint, not_before timestamptz,
           not_after timestamptz, first_seen date, last_seen date) ON COMMIT DROP",
        &[],
    )?;
    tx.execute("TRUNCATE stage_split", &[])?;

    // 1. Closures and attribute changes for objects in the DB current set.
    for (key, cur) in &current {
        match today.get(key) {
            Some(e) => {
                let window_drift = {
                    let nb = (e.not_before - cur.not_before).num_seconds().abs();
                    let na = (e.not_after - cur.not_after).num_seconds().abs();
                    nb < 129_600 && na < 129_600
                };
                if e.max_len != cur.max_len || !window_drift {
                    let (nb, na) = if window_drift && e.max_len == cur.max_len {
                        (cur.not_before, cur.not_after)
                    } else {
                        (e.not_before, e.not_after)
                    };
                    // Attribute change: close the version observed on `day`,
                    // keep the days after it as their own span, then write the
                    // day's attributes. The old tail matters when a repair
                    // re-applies an older day: later days were ingested with the
                    // previous attributes and must not inherit the new ones.
                    stage_span_tail(&mut tx, cur.roa_obj_id, cur.version_first_seen, day)?;
                    counts.versions_closed += tx.execute(
                        "UPDATE wayback.roa_version
                            SET last_seen = GREATEST(LEAST(COALESCE(last_seen, 'infinity'::date), $1), first_seen)
                          WHERE roa_obj_id = $2
                            AND COALESCE(last_seen, 'infinity'::date) >= $1::date + 1
                            AND first_seen <= $1::date + 1",
                        &[&prev_day, &cur.roa_obj_id],
                    )? as i64;
                    counts.versions_inserted += insert_staged_tail(&mut tx)?;
                    counts.versions_inserted += tx.execute(
                        "INSERT INTO wayback.roa_version
                           (roa_obj_id, max_len, not_before, not_after, first_seen, last_seen)
                         VALUES ($1, $2, $3::timestamp AT TIME ZONE 'UTC', $4::timestamp AT TIME ZONE 'UTC', $5,
                                 CASE WHEN (SELECT max(f.file_date) FROM wayback.source_file f
                                             WHERE f.tal = $6 AND f.artifact = 'roas.csv.xz'
                                               AND f.gap_class = 'observed') = $5
                                      THEN NULL ELSE $5 END)
                         ON CONFLICT (roa_obj_id, first_seen) DO UPDATE
                           SET max_len = EXCLUDED.max_len,
                               not_before = EXCLUDED.not_before,
                               not_after = EXCLUDED.not_after,
                               last_seen = EXCLUDED.last_seen",
                        &[
                            &cur.roa_obj_id,
                            &(e.max_len as i16),
                            &nb,
                            &na,
                            &day,
                            &tal,
                        ],
                    )? as i64;
                }
                // else: unchanged -> no write (NULL stays).
            }
            None if file_ok => {
                // Same TAL current row is absent from today's file. A row
                // observed only yesterday is a valid one-day span and must be
                // closed; a span that begins today has nothing left to close.
                if cur.version_first_seen >= day {
                    // The observation is this day itself and the file says the
                    // object is not there: the row has to go, keeping only a
                    // verified later tail.
                    stage_span_tail(&mut tx, cur.roa_obj_id, cur.version_first_seen, day)?;
                    counts.versions_closed += tx.execute(
                        "DELETE FROM wayback.roa_version
                          WHERE roa_obj_id = $1 AND first_seen = $2",
                        &[&cur.roa_obj_id, &cur.version_first_seen],
                    )? as i64;
                    counts.versions_inserted += insert_staged_tail(&mut tx)?;
                } else if let Some(last_seen) =
                    last_seen_before_absence(cur.version_first_seen, day)
                {
                    close_span_with_split(
                        &mut tx,
                        cur.roa_obj_id,
                        cur.version_first_seen,
                        day,
                        last_seen,
                        &mut counts,
                    )?;
                }
            }
            None => {}
        }
    }

    // 2. Inserts for entries not yet in the DB: COPY into a staging table,
    //    then set-based INSERT..SELECT (per-row round trips do not scale).
    {
        tx.execute(
            "CREATE TEMP TABLE IF NOT EXISTS stage_day (
               uri text, prefix text, origin bigint, max_len smallint,
               not_before timestamptz, not_after timestamptz) ON COMMIT DROP",
            &[],
        )?;
        tx.execute("TRUNCATE stage_day", &[])?;
        {
            let mut w = tx.copy_in(
                "COPY stage_day (uri, prefix, origin, max_len, not_before, not_after) FROM STDIN",
            )?;
            use std::io::Write;
            let mut buf = String::new();
            for (key, e) in &today {
                if current.contains_key(key) {
                    continue;
                }
                let line = format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\n",
                    escape_tsv(&e.uri),
                    escape_tsv(&e.prefix),
                    e.origin_asn,
                    e.max_len,
                    e.not_before.format("%Y-%m-%d %H:%M:%S+00"),
                    e.not_after.format("%Y-%m-%d %H:%M:%S+00"),
                );
                buf.push_str(&line);
                if buf.len() > 1 << 20 {
                    w.write_all(buf.as_bytes())?;
                    buf.clear();
                }
            }
            w.write_all(buf.as_bytes())?;
            w.finish()?;
        }
        // Objects: insert (or reopen) in bulk. `last_seen` is not written here:
        // the marker is derived from the version spans at the end of the day.
        let n_obj = tx.execute(
            "INSERT INTO wayback.roa_object (prefix, origin_asn, ta, uri, first_seen)
             SELECT DISTINCT s.prefix::cidr, s.origin, $1::text, s.uri, $2::date FROM stage_day s
             ON CONFLICT (ta, uri, prefix, origin_asn) DO UPDATE
               SET first_seen = LEAST(wayback.roa_object.first_seen, EXCLUDED.first_seen)",
            &[&tal, &day],
        )?;
        // Window alignment: ARIN (and others) shift certificate windows by a
        // few hours between snapshots (tz drift in publication). A staged row
        // whose window is within 36h of an existing span for the same object
        // and identical max_len is treated as the same certificate: adopt the
        // stored window so the coverage guard matches instead of inserting a
        // duplicate span.
        tx.execute(
            "UPDATE stage_day s
                SET not_before = v.not_before, not_after = v.not_after
               FROM wayback.roa_object o
               JOIN wayback.roa_version v ON v.roa_obj_id = o.roa_obj_id
              WHERE o.ta = $1 AND o.uri = s.uri AND o.prefix::text = s.prefix AND o.origin_asn = s.origin
                AND v.max_len = s.max_len
                AND abs(extract(epoch from (v.not_before - s.not_before))) < 129600
                AND abs(extract(epoch from (v.not_after - s.not_after))) < 129600",
            &[&tal],
        )?;
        // A repaired day next to an existing span continues that span: extend
        // the span that ends the day before `day` instead of opening a second
        // row for the same attributes. The extension reaches exactly one day:
        // nothing observed the days up to the next span, so claiming them would
        // assert presence the archive never showed.
        counts.spans_extended += tx.execute(
            "UPDATE wayback.roa_version v
                SET last_seen = CASE WHEN (SELECT max(f.file_date) FROM wayback.source_file f
                                           WHERE f.tal = $1 AND f.artifact = 'roas.csv.xz'
                                             AND f.gap_class = 'observed') = $2::date
                                     THEN NULL ELSE $2::date END
               FROM stage_day s
               JOIN wayback.roa_object o
                 ON o.ta = $1 AND o.uri = s.uri AND o.prefix::text = s.prefix AND o.origin_asn = s.origin
              WHERE v.roa_obj_id = o.roa_obj_id
                AND v.max_len = s.max_len
                AND v.not_before = s.not_before AND v.not_after = s.not_after
                AND v.last_seen = $2::date - 1",
            &[&tal, &day],
        )? as i64;
        // Versions: one per staged tuple unless a matching span actually covers
        // the day (idempotent replay). A span written for a historical day
        // covers that day only and stays open only when the day is the latest
        // one the TAL observed: a later span, or observed days after it, would
        // otherwise be filled in with presence no file ever showed.
        let n_ver = tx.execute(
            "INSERT INTO wayback.roa_version (roa_obj_id, max_len, not_before, not_after, first_seen, last_seen)
             SELECT o.roa_obj_id, d.max_len, d.not_before, d.not_after, $2::date,
                    CASE WHEN (SELECT max(f.file_date) FROM wayback.source_file f
                                WHERE f.tal = $1 AND f.artifact = 'roas.csv.xz'
                                  AND f.gap_class = 'observed') = $2::date
                         THEN NULL ELSE $2::date END
             FROM (
               SELECT DISTINCT ON (uri, prefix, origin) uri, prefix, origin, max_len, not_before, not_after
               FROM stage_day ORDER BY uri, prefix, origin, max_len
             ) d
             JOIN wayback.roa_object o
               ON o.ta = $1 AND o.uri = d.uri AND o.prefix::text = d.prefix AND o.origin_asn = d.origin
             WHERE NOT EXISTS (
               SELECT 1 FROM wayback.roa_version v
               WHERE v.roa_obj_id = o.roa_obj_id
                 AND v.max_len = d.max_len
                 AND v.not_before = d.not_before AND v.not_after = d.not_after
                 AND v.first_seen <= $2::date
                 AND (v.last_seen IS NULL OR v.last_seen >= $2::date)
             )",
            &[&tal, &day],
        )?;
        // Rewind: out-of-order backfill moves an existing current version's
        // first_seen back instead of creating an overlapping open-ended span.
        tx.execute(
            "UPDATE wayback.roa_version v
               SET first_seen = LEAST(v.first_seen, $2::date)
             FROM stage_day s
             JOIN wayback.roa_object o
               ON o.ta = $1 AND o.uri = s.uri AND o.prefix::text = s.prefix AND o.origin_asn = s.origin
            WHERE v.roa_obj_id = o.roa_obj_id
              AND v.max_len = s.max_len
              AND v.not_before = s.not_before AND v.not_after = s.not_after
              AND v.first_seen > $2::date
              AND NOT EXISTS (
                SELECT 1 FROM wayback.roa_version w
                WHERE w.roa_obj_id = v.roa_obj_id
                  AND w.max_len = s.max_len
                  AND w.not_before = s.not_before AND w.not_after = s.not_after
                  AND w.first_seen < v.first_seen
              )",
            &[&tal, &day],
        )?;
        // Close current versions whose attributes no longer match any staged row
        // for the same object (attribute change), then insert new versions.
        tx.execute(
            "UPDATE wayback.roa_version v
                SET last_seen = $2::date - 1
              WHERE v.last_seen IS NULL
                AND v.first_seen < $2::date
                AND EXISTS (
                  SELECT 1 FROM stage_day s
                  JOIN wayback.roa_object o
                    ON o.ta = $1 AND o.uri = s.uri AND o.prefix::text = s.prefix AND o.origin_asn = s.origin
                  WHERE o.roa_obj_id = v.roa_obj_id
                    AND (v.max_len <> s.max_len OR v.not_before <> s.not_before OR v.not_after <> s.not_after)
                )
                AND NOT EXISTS (
                  SELECT 1 FROM stage_day s2
                  JOIN wayback.roa_object o2
                    ON o2.ta = $1 AND o2.uri = s2.uri AND o2.prefix::text = s2.prefix AND o2.origin_asn = s2.origin
                  WHERE o2.roa_obj_id = v.roa_obj_id
                    AND v.max_len = s2.max_len AND v.not_before = s2.not_before AND v.not_after = s2.not_after
                )",
            &[&tal, &day],
        )?;
        counts.objects_inserted += n_obj as i64;
        counts.versions_inserted += n_ver as i64;
    }

    // 3. `roa_object.last_seen` is a denormalized current-state marker, not a
    //    second history axis: with out-of-order ingest the object can still
    //    hold an open span after an earlier day was re-applied, so derive the
    //    marker from the latest span of every touched object.
    let current_ids: Vec<i64> = current.values().map(|row| row.roa_obj_id).collect();
    // An object whose observations are all gone (a repair that removes its first
    // day) is no longer an object: drop the identity too, or the current view
    // keeps an object that holds no span.
    tx.execute(
        "DELETE FROM wayback.roa_object o
          WHERE o.roa_obj_id = ANY($1::bigint[])
            AND NOT EXISTS (SELECT 1 FROM wayback.roa_version v
                             WHERE v.roa_obj_id = o.roa_obj_id)",
        &[&current_ids],
    )?;
    counts.object_markers_updated += tx.execute(
        "WITH touched AS (
            SELECT o.roa_obj_id
            FROM stage_day s
            JOIN wayback.roa_object o
              ON o.ta = $1 AND o.uri = s.uri AND o.prefix::text = s.prefix AND o.origin_asn = s.origin
            UNION
            SELECT unnest($2::bigint[])
          ), latest AS (
            SELECT DISTINCT ON (v.roa_obj_id) v.roa_obj_id, v.last_seen
            FROM wayback.roa_version v
            JOIN touched t USING (roa_obj_id)
            ORDER BY v.roa_obj_id, v.first_seen DESC
          )
          UPDATE wayback.roa_object o
             SET last_seen = latest.last_seen
            FROM latest
           WHERE o.roa_obj_id = latest.roa_obj_id
             AND o.last_seen IS DISTINCT FROM latest.last_seen",
        &[&tal, &current_ids],
    )? as i64;

    tx.commit().context("commit transaction")?;
    Ok((
        counts.objects_inserted + counts.versions_inserted,
        counts.versions_closed + counts.spans_extended + counts.object_markers_updated,
    ))
}

#[derive(Default)]
pub(crate) struct RunCounts {
    pub(crate) files_ok: i64,
    pub(crate) files_failed: i64,
    pub(crate) rows_inserted: i64,
    pub(crate) rows_updated: i64,
}

/// Continue one day past the latest observed snapshot. An incremental walk stops
/// at the first day it cannot observe, so this cursor stays before a gap and the
/// next run retries it.
fn next_update_day(last_observed: Option<NaiveDate>) -> Result<NaiveDate> {
    last_observed
        .map(|day| day + chrono::Duration::days(1))
        .context("no observed source_file rows; run `wayback-pg backfill` first")
}

pub(crate) fn tal_from_url(tal_url: &str) -> &str {
    tal_url
        .trim_end_matches(".tal")
        .rsplit('/')
        .next()
        .unwrap_or("unknown")
}

/// Latest day this TAL's artifact was observed in the source ledger.
pub(crate) fn last_observed_day(
    client: &mut Client,
    tal: &str,
    artifact: &str,
) -> Result<Option<NaiveDate>> {
    client
        .query_one(
            "SELECT max(file_date) FROM wayback.source_file
             WHERE tal = $1 AND artifact = $2 AND gap_class = 'observed'",
            &[&tal, &artifact],
        )
        .map(|row| row.get(0))
        .context("look up latest observed snapshot")
}

pub(crate) fn start_run(client: &mut Client, mode: &str) -> Result<i64> {
    let locked: bool = client
        .query_one(
            "SELECT pg_try_advisory_lock(hashtext('wayback_ingest'))",
            &[],
        )?
        .get(0);
    if !locked {
        bail!("another wayback ingest is already running");
    }

    client
        .query_one(
            "INSERT INTO wayback.ingest_run (started_at, mode, code_version)
             VALUES (now(), $1, $2) RETURNING run_id",
            &[&mode, &format!("v2.1-{}", env!("CARGO_PKG_VERSION"))],
        )
        .map(|row| row.get(0))
        .context("start ingest run")
}

pub(crate) fn finish_run(client: &mut Client, run_id: i64, counts: &RunCounts) -> Result<()> {
    client.execute(
        "UPDATE wayback.ingest_run
         SET finished_at = now(), files_ok = $1, files_failed = $2,
             rows_inserted = $3, rows_updated = $4
         WHERE run_id = $5",
        &[
            &(counts.files_ok as i32),
            &(counts.files_failed as i32),
            &counts.rows_inserted,
            &counts.rows_updated,
            &run_id,
        ],
    )?;
    client.execute("SELECT pg_advisory_unlock(hashtext('wayback_ingest'))", &[])?;
    Ok(())
}

/// How a calendar day the archive does not list is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissingDays {
    /// Record the gap in the ledger and fail the run: at the frontier a gap has
    /// to be looked at.
    FailRun,
    /// Record the gap in the ledger without failing the run: a historical
    /// backfill legitimately walks days no TAL published yet.
    RecordOnly,
}

fn ingest_missing_day(
    client: &mut Client,
    tal: &str,
    day: NaiveDate,
    reason: &str,
    fail_run: bool,
    counts: &mut RunCounts,
) -> Result<()> {
    eprintln!("  MISSING {day}: {reason}");
    ingest_day(client, tal, day, &[], false, None, None)?;
    if fail_run {
        counts.files_failed += 1;
    }
    Ok(())
}

/// Apply one archive file. Returns whether the day counts as a run failure.
fn ingest_file(
    client: &mut Client,
    tal: &str,
    file: &crate::RoaFile,
    counts: &mut RunCounts,
) -> Result<bool> {
    let entries = match parse_roas_csv_full(&file.url) {
        Ok(entries) => entries,
        Err(error) => {
            ingest_missing_day(
                client,
                tal,
                file.file_date,
                &format!("fetch or parse {}: {error:#}", file.url),
                true,
                counts,
            )?;
            return Ok(true);
        }
    };

    let (inserted, updated) =
        ingest_day(client, tal, file.file_date, &entries, true, Some(200), None)?;
    counts.files_ok += 1;
    counts.rows_inserted += inserted;
    counts.rows_updated += updated;
    println!(
        "  {} rows={} rows_inserted={} rows_updated={}",
        file.file_date,
        entries.len(),
        inserted,
        updated
    );
    Ok(false)
}

/// Apply a known file list to one TAL's range. Split out of the crawl so the
/// calendar walk and its stop-at-first-gap policy are testable without the
/// network.
fn ingest_day_files(
    client: &mut Client,
    tal: &str,
    files: Vec<crate::RoaFile>,
    from: NaiveDate,
    until: NaiveDate,
    missing: MissingDays,
    counts: &mut RunCounts,
) -> Result<()> {
    let mut files_by_day = BTreeMap::new();
    for file in files {
        files_by_day.insert(file.file_date, file);
    }
    // Every calendar day of the range gets a ledger row, so "nothing to do"
    // stays distinguishable from "nothing was read".
    let mut day = from;
    while day <= until {
        match files_by_day.remove(&day) {
            Some(file) => {
                let failed = ingest_file(client, tal, &file, counts)?;
                if failed && missing == MissingDays::FailRun {
                    // The cursor is the latest observed day, so walking past a
                    // failure would skip it for good.
                    break;
                }
            }
            None => {
                ingest_missing_day(
                    client,
                    tal,
                    day,
                    "not listed by RIPE FTP",
                    missing == MissingDays::FailRun,
                    counts,
                )?;
                if missing == MissingDays::FailRun {
                    break;
                }
            }
        }
        day += chrono::Duration::days(1);
    }
    Ok(())
}

fn ingest_tal_range(
    client: &mut Client,
    tal_url: &str,
    from: NaiveDate,
    until: NaiveDate,
    missing: MissingDays,
    counts: &mut RunCounts,
) -> Result<()> {
    let tal = tal_from_url(tal_url);
    println!("== TAL {tal} ==");
    let mut files = try_crawl_tal_after(tal_url, Some(from), Some(until))?;
    files.sort_by_key(|file| file.file_date);
    ingest_day_files(client, tal, files, from, until, missing, counts)
}

pub(crate) fn run_ingest<F>(client: &mut Client, mode: &str, work: F) -> Result<()>
where
    F: FnOnce(&mut Client, &mut RunCounts) -> Result<()>,
{
    let run_id = start_run(client, mode)?;
    let mut counts = RunCounts::default();
    let work_result = work(client, &mut counts);
    let finish_result = finish_run(client, run_id, &counts);

    match (work_result, finish_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) if counts.files_failed > 0 => {
            bail!(
                "{mode} completed with {} failed file(s)",
                counts.files_failed
            )
        }
        (Ok(()), Ok(())) => {
            println!(
                "{mode} done: ok={} failed={}",
                counts.files_ok, counts.files_failed
            );
            Ok(())
        }
    }
}

/// Daily incremental ingest. Each TAL starts one day after its own most recent
/// observed source file, so a lagging or failed TAL cannot be skipped by a
/// newer snapshot from another TAL.
pub fn pg_update(config: &str, until: Option<NaiveDate>, tals: &[String]) -> Result<()> {
    let mut client = Client::connect(config, NoTls).context("connect to PostgreSQL")?;
    let until =
        until.unwrap_or_else(|| chrono::Utc::now().date_naive() - chrono::Duration::days(1));
    let tal_urls = selected_tal_urls(tals)?;

    run_ingest(&mut client, "update", |client, counts| {
        for tal_url in &tal_urls {
            let tal = tal_from_url(tal_url);
            let from = next_update_day(last_observed_day(client, tal, ROA_ARTIFACT)?)?;
            if from > until {
                println!("== TAL {tal} == already observed through {until}");
                continue;
            }
            ingest_tal_range(client, tal_url, from, until, MissingDays::FailRun, counts)?;
        }
        Ok(())
    })
}

/// Historical ingest over an explicit range. Days RIPE does not list are
/// recorded in the source ledger as gaps and do not fail the run; crawl, fetch,
/// and parse failures still do.
pub fn pg_backfill(config: &str, from: NaiveDate, until: NaiveDate, tals: &[String]) -> Result<()> {
    if from > until {
        bail!("--from must not be later than --until");
    }
    let mut client = Client::connect(config, NoTls).context("connect to PostgreSQL")?;
    let tal_urls = selected_tal_urls(tals)?;

    run_ingest(&mut client, "backfill", |client, counts| {
        for tal_url in &tal_urls {
            ingest_tal_range(
                client,
                tal_url,
                from,
                until,
                MissingDays::RecordOnly,
                counts,
            )?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod update_tests {
    use super::*;

    fn day(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").expect("valid test date")
    }

    #[test]
    fn next_update_day_requires_an_observed_snapshot() {
        assert_eq!(
            next_update_day(Some(day("2026-09-03"))).unwrap(),
            day("2026-09-04")
        );
        assert!(next_update_day(None).is_err());
    }

    #[test]
    fn absence_closes_an_object_seen_only_on_the_previous_day() {
        assert_eq!(
            last_seen_before_absence(day("2026-09-04"), day("2026-09-05")),
            Some(day("2026-09-04"))
        );
        assert_eq!(
            last_seen_before_absence(day("2026-09-05"), day("2026-09-05")),
            None
        );
    }

    #[test]
    fn rejects_a_source_without_a_roa_csv_header() {
        let path = std::env::temp_dir().join(format!(
            "wayback-rpki-missing-header-{}.csv",
            std::process::id()
        ));
        std::fs::write(&path, "<html>source error</html>\n").unwrap();

        let result = parse_roas_csv_full(path.to_str().unwrap());
        std::fs::remove_file(path).unwrap();

        assert!(result.is_err());
    }

    #[test]
    fn rejects_a_nonempty_invalid_max_length() {
        let path = std::env::temp_dir().join(format!(
            "wayback-rpki-invalid-max-len-{}.csv",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "URI,ASN,IP Prefix,Max Length,Not Before,Not After\nrsync://example,AS64496,192.0.2.0/24,invalid,2026-09-04 00:00:00,2026-09-05 00:00:00\n",
        )
        .unwrap();

        let result = parse_roas_csv_full(path.to_str().unwrap());
        std::fs::remove_file(path).unwrap();

        assert!(result.is_err());
    }
}

/// PostgreSQL integration tests share one database: `connect()` takes a session
/// advisory lock, resets the `wayback` schema, and returns `None` (skipping the
/// test) when `WAYBACK_PG_TEST_CONFIG` is unset.
#[cfg(test)]
pub(crate) mod test_db {
    use postgres::{Client, NoTls};

    pub(crate) fn connect() -> Option<Client> {
        let config = match std::env::var("WAYBACK_PG_TEST_CONFIG") {
            Ok(config) if !config.trim().is_empty() => config,
            _ => {
                eprintln!(
                    "skipping PostgreSQL test: set WAYBACK_PG_TEST_CONFIG to a disposable database"
                );
                return None;
            }
        };
        let mut client = Client::connect(&config, NoTls).expect("connect test database");
        client
            .execute("SELECT pg_advisory_lock(hashtext('wayback_pg_tests'))", &[])
            .expect("serialize PostgreSQL tests");
        client
            .batch_execute("DROP SCHEMA IF EXISTS wayback CASCADE")
            .expect("reset wayback schema");
        client
            .batch_execute(include_str!("../pg/001_schema.sql"))
            .expect("apply 001_schema.sql");
        client
            .batch_execute(include_str!("../pg/002_aspa.sql"))
            .expect("apply 002_aspa.sql");
        Some(client)
    }
}

#[cfg(test)]
mod pg_tests {
    use super::*;
    use crate::pg_ingest::test_db;

    fn day(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").expect("valid test date")
    }

    fn ts(value: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
            .expect("valid test timestamp")
    }

    fn roa(max_len: u8) -> RoaFullEntry {
        RoaFullEntry {
            uri: "rsync://rpki.example/roa/test.cer".to_string(),
            prefix: "192.0.2.0/24".to_string(),
            origin_asn: 64496,
            max_len,
            not_before: ts("2026-01-01 00:00:00"),
            not_after: ts("2027-01-01 00:00:00"),
        }
    }

    /// Observation spans of the stored object, oldest first.
    fn spans(client: &mut Client) -> Vec<(NaiveDate, Option<NaiveDate>)> {
        client
            .query(
                "SELECT first_seen, last_seen FROM wayback.roa_version ORDER BY first_seen",
                &[],
            )
            .expect("query spans")
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect()
    }

    fn object_marker(client: &mut Client) -> Option<NaiveDate> {
        client
            .query_one("SELECT last_seen FROM wayback.roa_object", &[])
            .expect("query object marker")
            .get(0)
    }

    fn table_count(client: &mut Client, sql: &str) -> i64 {
        client.query_one(sql, &[]).expect("count query").get(0)
    }

    fn gap_class(client: &mut Client, day: NaiveDate) -> String {
        client
            .query_one(
                "SELECT gap_class FROM wayback.source_file WHERE file_date = $1",
                &[&day],
            )
            .expect("ledger row")
            .get(0)
    }

    fn present(client: &mut Client, day: NaiveDate) {
        ingest_day(client, "test", day, &[roa(24)], true, Some(200), None)
            .expect("ingest present day");
    }

    #[test]
    fn unknown_tal_is_reported_instead_of_panicking() {
        let error = selected_tal_urls(&["apnic".to_string(), "zz".to_string()])
            .expect_err("an unknown TAL must be an error");
        assert!(error.to_string().contains("unknown TAL"), "{error:#}");
        assert_eq!(
            selected_tal_urls(&[]).expect("all TALs").len(),
            crate::tal_names().len()
        );
    }

    /// A readable ROA CSV on disk, so the calendar walk can be driven without
    /// the network.
    fn csv_fixture(name: &str) -> String {
        let path =
            std::env::temp_dir().join(format!("wayback-roa-{}-{name}.csv", std::process::id()));
        std::fs::write(
            &path,
            "URI,ASN,IP Prefix,Max Length,Not Before,Not After\n\
             rsync://rpki.example/roa/test.cer,AS64496,192.0.2.0/24,24,2026-01-01 00:00:00,2027-01-01 00:00:00\n",
        )
        .expect("write CSV fixture");
        path.to_string_lossy().into_owned()
    }

    fn roa_file(url: String, file_date: NaiveDate) -> crate::RoaFile {
        crate::RoaFile {
            tal: "test".to_string(),
            url,
            file_date,
            rows_count: 0,
            processed: false,
        }
    }

    #[test]
    fn an_incremental_walk_stops_at_the_first_failed_day() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        let second = day("2026-09-02");
        let third = day("2026-09-03");
        let missing_url = std::env::temp_dir()
            .join(format!("wayback-roa-{}-absent.csv", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let files = vec![
            roa_file(csv_fixture("first"), first),
            roa_file(missing_url, second),
            roa_file(csv_fixture("third"), third),
        ];
        let mut counts = RunCounts::default();

        ingest_day_files(
            &mut client,
            "test",
            files,
            first,
            third,
            MissingDays::FailRun,
            &mut counts,
        )
        .expect("walk the range");

        // The unreadable day is recorded, the walk stops there, and the cursor
        // still points at it so the next run retries it.
        assert_eq!(gap_class(&mut client, first), "observed");
        assert_eq!(gap_class(&mut client, second), "missing");
        assert_eq!(
            table_count(
                &mut client,
                "SELECT count(*) FROM wayback.source_file WHERE file_date = '2026-09-03'"
            ),
            0
        );
        assert_eq!(counts.files_failed, 1);
        let cursor =
            next_update_day(last_observed_day(&mut client, "test", ROA_ARTIFACT).expect("cursor"))
                .expect("cursor");
        assert_eq!(cursor, second);
    }

    #[test]
    fn a_backfill_walk_records_the_gap_and_continues() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        let second = day("2026-09-02");
        let third = day("2026-09-03");
        let missing_url = std::env::temp_dir()
            .join(format!("wayback-roa-{}-absent2.csv", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let files = vec![
            roa_file(csv_fixture("first"), first),
            roa_file(missing_url, second),
            roa_file(csv_fixture("third"), third),
        ];
        let mut counts = RunCounts::default();

        ingest_day_files(
            &mut client,
            "test",
            files,
            first,
            third,
            MissingDays::RecordOnly,
            &mut counts,
        )
        .expect("walk the range");

        assert_eq!(gap_class(&mut client, second), "missing");
        assert_eq!(gap_class(&mut client, third), "observed");
        assert_eq!(counts.files_failed, 1);
    }

    #[test]
    fn reapplying_the_first_day_as_absent_removes_the_observation() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let only_day = day("2026-09-01");
        present(&mut client, only_day);
        assert_eq!(
            table_count(&mut client, "SELECT count(*) FROM wayback.roa_version"),
            1
        );

        // The file for that very day is known not to carry the object.
        ingest_day(&mut client, "test", only_day, &[], true, Some(200), None)
            .expect("re-apply the first day as absent");

        assert_eq!(
            table_count(&mut client, "SELECT count(*) FROM wayback.roa_version"),
            0
        );
        assert_eq!(
            table_count(&mut client, "SELECT count(*) FROM wayback.roa_object"),
            0
        );
        assert_eq!(spans(&mut client), Vec::new());
    }

    #[test]
    fn reapplying_the_first_day_as_absent_keeps_the_later_tail() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        let second = day("2026-09-02");
        present(&mut client, first);
        present(&mut client, second);
        assert_eq!(spans(&mut client), vec![(first, None)]);

        ingest_day(&mut client, "test", first, &[], true, Some(200), None)
            .expect("re-apply the first day as absent");

        // Only the first day is withdrawn; the object is still current because
        // of the day that was observed after it.
        assert_eq!(spans(&mut client), vec![(second, None)]);
        assert_eq!(object_marker(&mut client), None);
        assert_eq!(
            table_count(&mut client, "SELECT count(*) FROM wayback.roa_object"),
            1
        );
    }

    #[test]
    fn a_repair_does_not_fill_days_observed_as_absent() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        let second = day("2026-09-02");
        let third = day("2026-09-03");
        let fourth = day("2026-09-04");
        let fifth = day("2026-09-05");
        present(&mut client, first);
        for absent_day in [second, third, fourth] {
            ingest_day(&mut client, "test", absent_day, &[], true, Some(200), None)
                .expect("absent day");
        }
        present(&mut client, fifth);
        assert_eq!(
            spans(&mut client),
            vec![(first, Some(first)), (fifth, None)]
        );

        // Day 2 was present after all, but days 3-4 were observed absent: the
        // repaired span must stop at day 2, not stretch to the next version.
        ingest_day(
            &mut client,
            "test",
            second,
            &[roa(24)],
            true,
            Some(200),
            None,
        )
        .expect("repair day 2");

        assert_eq!(
            spans(&mut client),
            vec![(first, Some(second)), (fifth, None)]
        );
    }

    #[test]
    fn a_repair_before_a_later_span_covers_only_its_day() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let second = day("2026-09-02");
        let fifth = day("2026-09-05");
        present(&mut client, fifth);
        assert_eq!(spans(&mut client), vec![(fifth, None)]);

        ingest_day(
            &mut client,
            "test",
            second,
            &[roa(24)],
            true,
            Some(200),
            None,
        )
        .expect("repair an earlier day");

        assert_eq!(
            spans(&mut client),
            vec![(second, Some(second)), (fifth, None)]
        );
    }

    #[test]
    fn an_attribute_change_repair_covers_only_its_day() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        let second = day("2026-09-02");
        let third = day("2026-09-03");
        present(&mut client, first);
        present(&mut client, second);
        ingest_day(&mut client, "test", third, &[], true, Some(200), None).expect("absent day");

        // Day 2 carried a different max_len after all, and day 3 was observed
        // absent: the new attribute belongs to day 2 only.
        ingest_day(
            &mut client,
            "test",
            second,
            &[roa(25)],
            true,
            Some(200),
            None,
        )
        .expect("repair day 2");

        assert_eq!(
            spans(&mut client),
            vec![(first, Some(first)), (second, Some(second))]
        );
        let max_lens: Vec<i16> = client
            .query(
                "SELECT max_len FROM wayback.roa_version ORDER BY first_seen",
                &[],
            )
            .expect("query max_len")
            .into_iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(max_lens, vec![24, 25]);
    }

    #[test]
    fn replaying_a_day_writes_nothing() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        present(&mut client, first);
        assert_eq!(spans(&mut client), vec![(first, None)]);

        let replay = ingest_day(
            &mut client,
            "test",
            first,
            &[roa(24)],
            true,
            Some(200),
            None,
        )
        .expect("replay");
        assert_eq!(replay, (0, 0));
        assert_eq!(spans(&mut client), vec![(first, None)]);
        assert_eq!(
            table_count(
                &mut client,
                "SELECT count(*) FROM wayback.roa_object_current_view"
            ),
            1
        );
    }

    #[test]
    fn an_attribute_change_closes_the_previous_span() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        let second = day("2026-09-02");
        present(&mut client, first);
        ingest_day(
            &mut client,
            "test",
            second,
            &[roa(25)],
            true,
            Some(200),
            None,
        )
        .expect("ingest changed attribute");

        assert_eq!(
            spans(&mut client),
            vec![(first, Some(first)), (second, None)]
        );
        assert_eq!(object_marker(&mut client), None);
    }

    #[test]
    fn an_absent_day_closes_the_span_and_the_object_marker() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        let second = day("2026-09-02");
        present(&mut client, first);
        // A readable file without the object: it disappeared on the second day.
        ingest_day(&mut client, "test", second, &[], true, Some(200), None)
            .expect("ingest absent day");

        assert_eq!(spans(&mut client), vec![(first, Some(first))]);
        assert_eq!(object_marker(&mut client), Some(first));
        assert_eq!(
            table_count(
                &mut client,
                "SELECT count(*) FROM wayback.roa_object_current_view"
            ),
            0
        );
    }

    #[test]
    fn reapplying_a_day_recorded_as_absent_continues_the_span() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        let second = day("2026-09-02");
        present(&mut client, first);
        ingest_day(&mut client, "test", second, &[], true, Some(200), None)
            .expect("ingest absent day");
        assert_eq!(spans(&mut client), vec![(first, Some(first))]);

        // The file for that day is now known to carry the object: the span that
        // ended the day before must gain the day back, not stay closed.
        let applied = ingest_day(
            &mut client,
            "test",
            second,
            &[roa(24)],
            true,
            Some(200),
            None,
        )
        .expect("re-apply the day");
        assert!(applied.1 > 0, "the re-applied day must write: {applied:?}");
        assert_eq!(spans(&mut client), vec![(first, None)]);
        assert_eq!(object_marker(&mut client), None);
        assert_eq!(gap_class(&mut client, second), "observed");
    }

    #[test]
    fn repairing_an_absent_middle_day_keeps_the_later_span() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let days: Vec<NaiveDate> = ["2026-09-01", "2026-09-02", "2026-09-03", "2026-09-04"]
            .iter()
            .map(|value| day(value))
            .collect();
        for present_day in &days {
            present(&mut client, *present_day);
        }
        assert_eq!(spans(&mut client), vec![(days[0], None)]);

        // Day 2 was not published after all: day 1 stays observed, days 3-4 keep
        // their coverage, and the object is still current because of them.
        ingest_day(&mut client, "test", days[1], &[], true, Some(200), None)
            .expect("repair the middle day");

        assert_eq!(
            spans(&mut client),
            vec![(days[0], Some(days[0])), (days[2], None)]
        );
        assert_eq!(object_marker(&mut client), None);
    }

    #[test]
    fn a_missing_file_is_not_reported_as_a_zero_count() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let only_day = day("2026-09-01");
        ingest_day(&mut client, "test", only_day, &[], false, None, None).expect("missing day");

        assert_eq!(
            table_count(&mut client, "SELECT count(*) FROM wayback.roa_counts_view"),
            0
        );
        assert_eq!(gap_class(&mut client, only_day), "missing");
    }

    #[test]
    fn ingesting_one_tal_leaves_another_tal_alone() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        let second = day("2026-09-02");
        // The same tuple authorized under two TALs: one TAL's absence must not
        // close the other TAL's object.
        for tal in ["test-a", "test-b"] {
            ingest_day(&mut client, tal, first, &[roa(24)], true, Some(200), None)
                .expect("first day");
            ingest_day(&mut client, tal, second, &[roa(24)], true, Some(200), None)
                .expect("second day");
        }
        ingest_day(&mut client, "test-a", second, &[], true, Some(200), None)
            .expect("test-a absent on the second day");

        let markers: Vec<(String, Option<NaiveDate>)> = client
            .query(
                "SELECT ta, last_seen FROM wayback.roa_object ORDER BY ta",
                &[],
            )
            .expect("query object markers")
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        assert_eq!(
            markers,
            vec![
                ("test-a".to_string(), Some(first)),
                ("test-b".to_string(), None)
            ]
        );
        let open_spans: i64 = client
            .query_one(
                "SELECT count(*) FROM wayback.roa_object o
                   JOIN wayback.roa_version v USING (roa_obj_id)
                  WHERE o.ta = 'test-b' AND v.last_seen IS NULL",
                &[],
            )
            .expect("query test-b spans")
            .get(0);
        assert_eq!(open_spans, 1);
    }

    #[test]
    fn repairing_an_attribute_change_keeps_the_later_span() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let days: Vec<NaiveDate> = ["2026-09-01", "2026-09-02", "2026-09-03", "2026-09-04"]
            .iter()
            .map(|value| day(value))
            .collect();
        for present_day in &days {
            present(&mut client, *present_day);
        }
        assert_eq!(spans(&mut client), vec![(days[0], None)]);

        // Day 2 is re-applied with a different max_len: only that day changes.
        // Days 3-4 were observed with the old attribute and keep it.
        ingest_day(
            &mut client,
            "test",
            days[1],
            &[roa(25)],
            true,
            Some(200),
            None,
        )
        .expect("repair day 2");

        assert_eq!(
            spans(&mut client),
            vec![
                (days[0], Some(days[0])),
                (days[1], Some(days[1])),
                (days[2], None)
            ]
        );
        let max_lens: Vec<i16> = client
            .query(
                "SELECT max_len FROM wayback.roa_version ORDER BY first_seen",
                &[],
            )
            .expect("query max_len")
            .into_iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(max_lens, vec![24, 25, 24]);
        assert_eq!(object_marker(&mut client), None);
    }

    #[test]
    fn reapplying_a_day_with_new_attributes_replaces_its_own_row() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        let second = day("2026-09-02");
        // The span starts on the second day, so the repair has to replace that
        // row instead of finding the primary key taken.
        ingest_day(&mut client, "test", first, &[], true, Some(200), None).expect("day 1 absent");
        present(&mut client, second);
        assert_eq!(spans(&mut client), vec![(second, None)]);

        ingest_day(
            &mut client,
            "test",
            second,
            &[roa(25)],
            true,
            Some(200),
            None,
        )
        .expect("re-apply day 2");

        assert_eq!(spans(&mut client), vec![(second, None)]);
        let max_len: i16 = client
            .query_one("SELECT max_len FROM wayback.roa_version", &[])
            .expect("query max_len")
            .get(0);
        assert_eq!(max_len, 25);
    }

    #[test]
    fn the_tuple_view_folds_objects_that_share_a_tuple() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        let second = day("2026-09-02");
        // Two objects (different certificate URIs) authorize the same tuple on
        // both days: the tuple view must still return one contiguous row.
        for uri in [
            "rsync://rpki.example/roa/a.cer",
            "rsync://rpki.example/roa/b.cer",
        ] {
            for each_day in [first, second] {
                let entry = RoaFullEntry {
                    uri: uri.to_string(),
                    ..roa(24)
                };
                ingest_day(
                    &mut client,
                    "test",
                    each_day,
                    &[entry],
                    true,
                    Some(200),
                    None,
                )
                .expect("ingest shared tuple");
            }
        }

        let islands: Vec<(NaiveDate, Option<NaiveDate>)> = client
            .query(
                "SELECT first_seen, last_seen FROM wayback.roa_tuple_view
                  WHERE prefix = '192.0.2.0/24' ORDER BY first_seen",
                &[],
            )
            .expect("query tuple view")
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        assert_eq!(islands, vec![(first, None)]);
    }

    #[test]
    fn the_tuple_view_keeps_disjoint_spans_apart() {
        let Some(mut client) = test_db::connect() else {
            return;
        };
        let first = day("2026-09-01");
        let second = day("2026-09-02");
        let third = day("2026-09-03");
        let fourth = day("2026-09-04");
        present(&mut client, first);
        ingest_day(&mut client, "test", second, &[], true, Some(200), None).expect("absent day");
        ingest_day(&mut client, "test", third, &[], true, Some(200), None).expect("absent day");
        present(&mut client, fourth);

        let islands: Vec<(NaiveDate, Option<NaiveDate>)> = client
            .query(
                "SELECT first_seen, last_seen FROM wayback.roa_tuple_view
                  WHERE prefix = '192.0.2.0/24' ORDER BY first_seen",
                &[],
            )
            .expect("query tuple view")
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        assert_eq!(islands, vec![(first, Some(first)), (fourth, None)]);
    }
}
