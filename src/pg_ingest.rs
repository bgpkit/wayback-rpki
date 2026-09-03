//! v2.1 PostgreSQL backfill (PoC): object-grain ROA history.
//!
//! Ingest contract (per (tal, day D), one transaction):
//! - new (ta, uri, prefix, origin)          -> INSERT object + first version row
//! - present, attributes unchanged          -> no write at all
//! - same URI, max_len or cert window moved -> close old version (last_seen=D-1),
//!                                             open new version (first_seen=D)
//! - in current set but absent from file    -> close object (last_seen=D-1)
//! - absent file (HTTP failure / no file)   -> no disappearance decisions; ledger row only
//!
//! Idempotent: replaying a day is a no-op.

use anyhow::{Context, Result};
use chrono::NaiveDate;
use postgres::{Client, NoTls};
use std::collections::HashMap;

use crate::{crawl_tal_after, get_tal_urls};

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

/// Parse a `roas.csv.xz` file (already downloaded to a local path by oneio)
/// keeping URI, Not Before, Not After. Accepts both the quoted-URI legacy
/// variant (pre-2018) and the current unquoted one.
pub fn parse_roas_csv_full(path: &str) -> Result<Vec<RoaFullEntry>> {
    let mut out = Vec::new();
    let mut header_seen = false;
    for line in oneio::read_lines_lossy(path)? {
        let line = line.unwrap_or_default();
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
            max_len: fields[3].parse().unwrap_or_else(|_| {
                // Max Length column may be empty -> defaults to prefix length
                fields[2]
                    .split('/')
                    .nth(1)
                    .and_then(|l| l.parse().ok())
                    .unwrap_or(0)
            }),
            not_before,
            not_after,
        });
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
                    v.max_len, v.not_before, v.not_after
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
                max_len: max_len as u8,
                not_before,
                not_after,
            },
        );
    }
    Ok(map)
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
    run_id: i64,
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
        today.insert(
            (
                tal.to_string(),
                e.uri.clone(),
                e.prefix.clone(),
                e.origin_asn as i64,
            ),
            e,
        );
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
                if e.max_len != cur.max_len
                    || e.not_before != cur.not_before
                    || e.not_after != cur.not_after
                {
                    // Attribute change: close old version, open new one.
                    tx.execute(
                        "UPDATE wayback.roa_version SET last_seen = GREATEST($1, first_seen)
                         WHERE roa_obj_id = $2 AND last_seen IS NULL AND first_seen <= $1::date + 1",
                        &[&prev_day, &cur.roa_obj_id],
                    )?;
                    tx.execute(
                        "INSERT INTO wayback.roa_version
                           (roa_obj_id, max_len, not_before, not_after, first_seen)
                         VALUES ($1, $2, $3::timestamp AT TIME ZONE 'UTC', $4::timestamp AT TIME ZONE 'UTC', $5)
                         ON CONFLICT (roa_obj_id, first_seen) DO NOTHING",
                        &[
                            &cur.roa_obj_id,
                            &(e.max_len as i16),
                            &e.not_before,
                            &e.not_after,
                            &day,
                        ],
                    )?;
                    counts.versions_closed += 1;
                    counts.versions_inserted += 1;
                }
                // else: unchanged -> no write (NULL stays).
            }
            None => {
                if file_ok && key.0 == tal {
                    // Same TAL current row absent from today's file: disappeared.
                    // (Rows of other TALs never appear in this file by construction.)
                    tx.execute(
                        "UPDATE wayback.roa_object SET last_seen = $1
                         WHERE roa_obj_id = $2 AND last_seen IS NULL AND first_seen < $1",
                        &[&prev_day, &cur.roa_obj_id],
                    )?;
                    tx.execute(
                        "UPDATE wayback.roa_version SET last_seen = $1
                         WHERE roa_obj_id = $2 AND last_seen IS NULL AND first_seen < $1",
                        &[&prev_day, &cur.roa_obj_id],
                    )?;
                    counts.objects_closed += 1;
                }
            }
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
        // Rewind: out-of-order backfill moves an existing current version's
        // first_seen back instead of creating an overlapping open-ended span.
        tx.execute(
            "UPDATE wayback.roa_version v
               SET first_seen = LEAST(v.first_seen, $2::date)
             FROM stage_day s
             JOIN wayback.roa_object o
               ON o.ta = $1 AND o.uri = s.uri AND o.prefix::text = s.prefix AND o.origin_asn = s.origin
            WHERE v.roa_obj_id = o.roa_obj_id AND v.last_seen IS NULL
              AND v.max_len = s.max_len
              AND v.not_before = s.not_before AND v.not_after = s.not_after
              AND v.first_seen > $2::date",
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
        // Versions: one per staged tuple unless an identical current version
        // already exists (idempotent replay).
        let n_ver = tx.execute(
            "INSERT INTO wayback.roa_version (roa_obj_id, max_len, not_before, not_after, first_seen)
             SELECT o.roa_obj_id, d.max_len, d.not_before, d.not_after, $2
             FROM (
               SELECT DISTINCT ON (uri, prefix, origin) uri, prefix, origin, max_len, not_before, not_after
               FROM stage_day ORDER BY uri, prefix, origin
             ) d
             JOIN wayback.roa_object o
               ON o.ta = $1 AND o.uri = d.uri AND o.prefix::text = d.prefix AND o.origin_asn = d.origin
             WHERE NOT EXISTS (
               SELECT 1 FROM wayback.roa_version v
               WHERE v.roa_obj_id = o.roa_obj_id AND v.last_seen IS NULL
             )",
            &[&tal, &day],
        )?;
        counts.objects_inserted = n_obj as i64;
        counts.versions_inserted = n_ver as i64;
    }

    // 3. Run counters.
    let rows_touched = counts.objects_inserted
        + counts.versions_inserted
        + counts.versions_closed
        + counts.objects_closed;
    tx.execute(
        "INSERT INTO wayback.ingest_run (started_at, mode, code_version, files_ok, files_failed, rows_inserted, rows_updated)
         VALUES (now(), 'backfill', $1, 1, 0, $2, $3)",
        &[&format!("poc-{}", env!("CARGO_PKG_VERSION")), &counts.objects_inserted, &rows_touched],
    )?;
    let _ = run_id;
    tx.commit().context("commit transaction")?;
    Ok((
        counts.objects_inserted + counts.versions_inserted,
        counts.versions_closed + counts.objects_closed,
    ))
}

/// Backfill a date range (per TAL, strictly chronological; TALs sequential in this PoC).
pub fn pg_backfill(
    config: &str,
    from: NaiveDate,
    until: NaiveDate,
    tals: Option<Vec<String>>,
) -> Result<()> {
    let mut client = Client::connect(config, NoTls).context("connect to PostgreSQL")?;

    let run = client
        .query_one(
            "INSERT INTO wayback.ingest_run (started_at, mode, code_version) VALUES (now(), 'backfill', $1) RETURNING run_id",
            &[&format!("poc-{}", env!("CARGO_PKG_VERSION"))],
        )?;
    let run_id: i64 = run.get(0);

    let tal_urls = get_tal_urls(tals.map(|t| t.join(",")));
    let mut files_ok = 0i64;
    let mut files_failed = 0i64;

    for tal_url in tal_urls {
        let tal = tal_url
            .trim_end_matches(".tal")
            .rsplit('/')
            .next()
            .unwrap_or("unknown")
            .to_string();
        println!("== TAL {tal} ==");
        let mut files = crawl_tal_after(&tal_url, Some(from), Some(until));
        files.sort_by_key(|f| f.file_date);
        for f in files {
            let url = f.url.to_string();
            // Download to a local cache file, parse full-fidelity, ingest.
            let local = format!(
                "/tmp/wayback_poc/{}_{}.csv.xz",
                tal,
                f.file_date.format("%Y%m%d")
            );
            std::fs::create_dir_all("/tmp/wayback_poc")?;
            let fetched = if std::path::Path::new(&local).exists() {
                true
            } else {
                match oneio::download(&url, &local) {
                    Ok(_) => true,
                    Err(e) => {
                        eprintln!("  {} download failed: {e}", f.file_date);
                        false
                    }
                }
            };
            if !fetched {
                let _ = ingest_day(
                    &mut client,
                    &tal,
                    f.file_date,
                    &[],
                    false,
                    Some(404),
                    None,
                    run_id,
                );
                files_failed += 1;
                continue;
            }
            let entries = parse_roas_csv_full(&local).with_context(|| format!("parse {local}"))?;
            let (ins, closed) = ingest_day(
                &mut client,
                &tal,
                f.file_date,
                &entries,
                true,
                Some(200),
                None,
                run_id,
            )?;
            files_ok += 1;
            println!(
                "  {} rows={} new_obj+ver={} closed={}",
                f.file_date,
                entries.len(),
                ins,
                closed
            );
        }
    }

    client.execute(
        "UPDATE wayback.ingest_run SET finished_at = now(), files_ok = $1, files_failed = $2 WHERE run_id = $3",
        &[&(files_ok as i32), &(files_failed as i32), &run_id],
    )?;
    println!("backfill done: ok={files_ok} failed={files_failed}");
    Ok(())
}
