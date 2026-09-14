//! v2.1 PostgreSQL backfill (PoC): object-grain ROA history.
//!
//! Ingest contract (per (tal, day D), one transaction):
//! - new (ta, uri, prefix, origin)          -> INSERT object + first version row
//! - present, attributes unchanged          -> no write at all
//! - same URI, max_len or cert window moved -> close old version (last_seen=D-1),
//!   open new version (first_seen=D)
//! - in current set but absent from file    -> close object (last_seen=D-1)
//! - absent file (HTTP failure / no file)   -> no disappearance decisions; ledger row only
//!
//! Idempotent: replaying a day is a no-op.

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use postgres::{Client, NoTls};
use std::collections::HashMap;

use crate::{crawl_tal_after, ROA_ARTIFACT};

/// Resolve the requested TAL names to archive URLs. The v1 helper takes a
/// single name and panics on an unknown one; the pg binary needs a validated
/// list, so names are resolved one by one and reported as an error.
pub(crate) fn selected_tal_urls(tals: &[String]) -> Result<Vec<String>> {
    if tals.is_empty() {
        return Ok(crate::get_tal_urls(None));
    }
    let mut urls = Vec::with_capacity(tals.len());
    for tal in tals {
        let resolved = crate::get_tal_urls(Some(tal.clone()));
        let Some(url) = resolved.into_iter().next() else {
            bail!("unknown TAL {tal:?}");
        };
        if !urls.contains(&url) {
            urls.push(url);
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
    object_first_seen: NaiveDate,
    version_first_seen: NaiveDate,
    max_len: u8,
    not_before: chrono::NaiveDateTime,
    not_after: chrono::NaiveDateTime,
}

struct IngestCounts {
    objects_inserted: i64,
    versions_inserted: i64,
    versions_closed: i64,
    objects_closed: i64,
}

/// Load all current objects (last_seen IS NULL) keyed by (ta, uri, prefix, origin).
fn load_current_objects(
    client: &mut postgres::Transaction,
    day: NaiveDate,
) -> Result<HashMap<(String, String, String, i64), CurrentVersion>> {
    let mut map = HashMap::new();
    // The version that was current ON `day`: latest version whose span covers
    // `day` (open-ended counts as covering). This makes day-D replay idempotent
    // even after later days were ingested.
    for row in client
        .query(
            "SELECT o.roa_obj_id, o.prefix::text, o.origin_asn, o.ta, o.uri,
                    v.max_len, v.not_before, v.not_after, o.first_seen, v.first_seen
             FROM wayback.roa_object o
             JOIN wayback.roa_version v ON v.roa_obj_id = o.roa_obj_id
             WHERE v.first_seen <= $1 AND (v.last_seen IS NULL OR v.last_seen >= $1)",
            &[&day],
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
                object_first_seen: row.get(8),
                version_first_seen: row.get(9),
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

    // Ledger row first (idempotent upsert).
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
            &(entries.len() as i32),
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

    let current = load_current_objects(&mut tx, day)?;
    let mut counts = IngestCounts {
        objects_inserted: 0,
        versions_inserted: 0,
        versions_closed: 0,
        objects_closed: 0,
    };

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
                    // Attribute change: close old version, open new one. Trim
                    // any span still covering `day` (open or closed) back to
                    // prev_day so the new span starting at `day` cannot overlap.
                    counts.versions_closed += tx.execute(
                        "UPDATE wayback.roa_version
                            SET last_seen = GREATEST(LEAST(COALESCE(last_seen, 'infinity'::date), $1), first_seen)
                          WHERE roa_obj_id = $2
                            AND COALESCE(last_seen, 'infinity'::date) >= $1::date + 1
                            AND first_seen <= $1::date + 1",
                        &[&prev_day, &cur.roa_obj_id],
                    )? as i64;
                    counts.versions_inserted += tx.execute(
                        "INSERT INTO wayback.roa_version
                           (roa_obj_id, max_len, not_before, not_after, first_seen, last_seen)
                         VALUES ($1, $2, $3::timestamp AT TIME ZONE 'UTC', $4::timestamp AT TIME ZONE 'UTC', $5,
                                 (SELECT min(w.first_seen) - 1 FROM wayback.roa_version w
                                   WHERE w.roa_obj_id = $1 AND w.first_seen > $5))
                         ON CONFLICT (roa_obj_id, first_seen) DO NOTHING",
                        &[
                            &cur.roa_obj_id,
                            &(e.max_len as i16),
                            &nb,
                            &na,
                            &day,
                        ],
                    )? as i64;
                }
                // else: unchanged -> no write (NULL stays).
            }
            None if file_ok && key.0 == tal => {
                // Same TAL current row is absent from today's file. A row
                // observed only yesterday is a valid one-day span and must be
                // closed, while a span that begins today cannot be rewound.
                if let Some(last_seen) = last_seen_before_absence(cur.object_first_seen, day) {
                    counts.objects_closed += tx.execute(
                        "UPDATE wayback.roa_object SET last_seen = $1
                         WHERE roa_obj_id = $2 AND last_seen IS NULL",
                        &[&last_seen, &cur.roa_obj_id],
                    )? as i64;
                }
                if let Some(last_seen) = last_seen_before_absence(cur.version_first_seen, day) {
                    counts.versions_closed += tx.execute(
                        "UPDATE wayback.roa_version SET last_seen = $1
                         WHERE roa_obj_id = $2 AND last_seen IS NULL",
                        &[&last_seen, &cur.roa_obj_id],
                    )? as i64;
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
        // Objects: insert (or reopen) in bulk.
        let n_obj = tx.execute(
            "INSERT INTO wayback.roa_object (prefix, origin_asn, ta, uri, first_seen)
             SELECT DISTINCT s.prefix::cidr, s.origin, $1::text, s.uri, $2::date FROM stage_day s
             ON CONFLICT (ta, uri, prefix, origin_asn) DO UPDATE
               SET last_seen = NULL,
                   first_seen = LEAST(wayback.roa_object.first_seen, EXCLUDED.first_seen)",
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
        // Versions: one per staged tuple unless a matching span already covers
        // the day (idempotent replay). When backfilling an earlier day
        // (out-of-order repair), bound the new span by the object's next
        // existing span instead of leaving it open-ended.
        let n_ver = tx.execute(
            "INSERT INTO wayback.roa_version (roa_obj_id, max_len, not_before, not_after, first_seen, last_seen)
             SELECT o.roa_obj_id, d.max_len, d.not_before, d.not_after, $2::date,
                    (SELECT min(w.first_seen) - 1 FROM wayback.roa_version w
                      WHERE w.roa_obj_id = o.roa_obj_id AND w.first_seen > $2::date)
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
                 AND (v.last_seen IS NULL OR v.last_seen >= $2::date - 1)
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
        counts.objects_inserted = n_obj as i64;
        counts.versions_inserted = n_ver as i64;
    }

    tx.commit().context("commit transaction")?;
    Ok((
        counts.objects_inserted + counts.versions_inserted,
        counts.versions_closed + counts.objects_closed,
    ))
}

#[derive(Default)]
pub(crate) struct RunCounts {
    pub(crate) files_ok: i64,
    pub(crate) files_failed: i64,
    pub(crate) rows_inserted: i64,
    pub(crate) rows_updated: i64,
}

fn next_update_day(last_observed: Option<NaiveDate>) -> Result<NaiveDate> {
    last_observed
        .map(|day| day + chrono::Duration::days(1))
        .context("no observed source_file rows; run `wayback backfill` first")
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

fn ingest_missing_day(
    client: &mut Client,
    tal: &str,
    day: NaiveDate,
    reason: &str,
    counts: &mut RunCounts,
) -> Result<()> {
    eprintln!("  MISSING {day}: {reason}");
    ingest_day(client, tal, day, &[], false, None, None)?;
    counts.files_failed += 1;
    Ok(())
}

fn ingest_file(
    client: &mut Client,
    tal: &str,
    file: &crate::RoaFile,
    counts: &mut RunCounts,
) -> Result<()> {
    let entries = match parse_roas_csv_full(&file.url) {
        Ok(entries) => entries,
        Err(error) => {
            return ingest_missing_day(
                client,
                tal,
                file.file_date,
                &format!("fetch or parse {}: {error:#}", file.url),
                counts,
            );
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
    Ok(())
}

fn ingest_tal_range(
    client: &mut Client,
    tal_url: &str,
    from: NaiveDate,
    until: NaiveDate,
    require_every_day: bool,
    counts: &mut RunCounts,
) -> Result<()> {
    let tal = tal_from_url(tal_url);
    println!("== TAL {tal} ==");
    let mut files = crawl_tal_after(tal_url, Some(from), Some(until));
    files.sort_by_key(|file| file.file_date);

    if !require_every_day {
        for file in files {
            ingest_file(client, tal, &file, counts)?;
        }
        return Ok(());
    }

    let mut files_by_day = std::collections::BTreeMap::new();
    for file in files {
        files_by_day.insert(file.file_date, file);
    }
    let mut day = from;
    while day <= until {
        match files_by_day.remove(&day) {
            Some(file) => ingest_file(client, tal, &file, counts)?,
            None => ingest_missing_day(client, tal, day, "not listed by RIPE FTP", counts)?,
        }
        day += chrono::Duration::days(1);
    }
    Ok(())
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
            ingest_tal_range(client, tal_url, from, until, true, counts)?;
        }
        Ok(())
    })
}

/// Historical ingest over an explicit range. Missing historical days are
/// recorded in the source ledger but do not fail the run merely because RIPE
/// does not list them; source and parser failures still do.
pub fn pg_backfill(config: &str, from: NaiveDate, until: NaiveDate, tals: &[String]) -> Result<()> {
    if from > until {
        bail!("--from must not be later than --until");
    }
    let mut client = Client::connect(config, NoTls).context("connect to PostgreSQL")?;
    let tal_urls = selected_tal_urls(tals)?;

    run_ingest(&mut client, "backfill", |client, counts| {
        for tal_url in &tal_urls {
            ingest_tal_range(client, tal_url, from, until, false, counts)?;
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
