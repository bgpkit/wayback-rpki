//! ASPA ingest: RIPE's daily `output.json.xz` snapshots into
//! `wayback.aspa_object` / `wayback.aspa_version`.
//!
//! An ASPA object is `(customer AS, provider set)` and is replaced whole, so the
//! store is ASN-keyed SCD-2 and writes a row only when the set changes. The
//! archive's ASPA history starts on the day the JSON artifact itself appeared
//! (2023-10-11); earlier days are recorded once per TAL as `era_start` rather
//! than read as publication gaps.

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use postgres::{Client, NoTls};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

use crate::pg_ingest::selected_tal_urls;
use crate::pg_ingest::{last_observed_day, run_ingest, tal_from_url, RunCounts};
use crate::{crawl_tal_artifact, ArchiveFile};

/// Ledger artifact that carries ASPA objects (Routinator JSON).
pub const ASPA_ARTIFACT: &str = "output.json.xz";

/// First archive day with an `output.json.xz` artifact anywhere. ASPA coverage
/// is bounded by this, not by the ROA CSV start (2015-03-10).
pub fn aspa_era_start() -> NaiveDate {
    NaiveDate::from_ymd_opt(2023, 10, 11).expect("valid ASPA era start")
}

/// Only the fields we consume; `roas` (hundreds of thousands of entries) and
/// `metadata` are skipped by serde instead of being deserialized.
#[derive(Debug, Deserialize)]
struct OutputJson {
    #[serde(default)]
    aspas: Vec<RawAspa>,
    #[serde(default, rename = "routerKeys")]
    router_keys: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RawAspa {
    customer: String,
    #[serde(default)]
    providers: Vec<String>,
}

/// One ASPA object with its canonical provider set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AspaEntry {
    pub customer_asn: i64,
    /// Ascending, deduplicated; AS0 only ever appears alone.
    pub providers: Vec<i64>,
    pub has_as0: bool,
    pub as0_only: bool,
}

/// Publisher deviations from the canonical form the profile requires. They are
/// counted and reported, never silently repaired: a nonzero rate is a finding
/// about the publisher, not something to hide.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AspaQuality {
    pub objects: u64,
    pub non_ascending_or_duplicate: u64,
    pub customer_in_own_set: u64,
    pub as0_alongside_others: u64,
    pub router_keys: u64,
}

impl AspaQuality {
    /// Total publisher-side deviations seen in the file.
    pub fn deviations(&self) -> u64 {
        self.non_ascending_or_duplicate + self.customer_in_own_set + self.as0_alongside_others
    }
}

fn parse_asn(value: &str) -> Result<i64> {
    let digits = value.trim().trim_start_matches("AS").trim();
    let asn: i64 = digits
        .parse()
        .with_context(|| format!("bad ASN {value:?}"))?;
    if !(0..=u32::MAX as i64).contains(&asn) {
        bail!("ASN out of range: {value:?}");
    }
    Ok(asn)
}

/// Parse a RIPE `output.json.xz` source URL or local path.
pub fn parse_output_json_aspas(path: &str) -> Result<(Vec<AspaEntry>, AspaQuality)> {
    let reader = oneio::get_reader(path).with_context(|| format!("open {path}"))?;
    let document: OutputJson =
        serde_json::from_reader(reader).with_context(|| format!("parse {path}"))?;

    let mut quality = AspaQuality {
        router_keys: document.router_keys.len() as u64,
        ..AspaQuality::default()
    };
    let mut entries = Vec::with_capacity(document.aspas.len());
    for raw in &document.aspas {
        let customer_asn = parse_asn(&raw.customer)?;
        let mut providers = Vec::with_capacity(raw.providers.len());
        for provider in &raw.providers {
            providers.push(parse_asn(provider)?);
        }
        if providers.windows(2).any(|pair| pair[0] >= pair[1]) {
            quality.non_ascending_or_duplicate += 1;
        }
        if providers.contains(&customer_asn) {
            quality.customer_in_own_set += 1;
        }
        let has_as0 = providers.contains(&0);
        if has_as0 && providers.len() > 1 {
            quality.as0_alongside_others += 1;
        }
        providers.sort_unstable();
        providers.dedup();
        entries.push(AspaEntry {
            customer_asn,
            as0_only: has_as0 && providers.len() == 1,
            has_as0,
            providers,
        });
    }
    quality.objects = entries.len() as u64;
    Ok((entries, quality))
}

struct CurrentAspa {
    aspa_obj_id: i64,
    object_first_seen: NaiveDate,
    version_first_seen: NaiveDate,
    providers: Vec<i64>,
}

/// One day's ASPA file for one TAL.
pub struct AspaFile<'a> {
    pub tal: &'a str,
    pub day: NaiveDate,
    pub entries: &'a [AspaEntry],
    pub quality: &'a AspaQuality,
    pub file_ok: bool,
    pub http_status: Option<i16>,
    pub sha256: Option<&'a str>,
    /// The day lies before this TAL's first observed ASPA artifact.
    pub era_start: bool,
}

/// Apply one day of ASPA objects for one TAL. `file_ok = false` records the gap
/// and makes no disappearance decisions, so spans stay open across it.
pub fn ingest_aspa_day(client: &mut Client, file: &AspaFile<'_>) -> Result<(i64, i64)> {
    let day = file.day;
    let prev_day = day - chrono::Duration::days(1);
    let mut tx = client.transaction().context("begin transaction")?;

    let gap_class = if file.file_ok {
        "observed"
    } else if file.era_start {
        "era_start"
    } else {
        "missing"
    };
    tx.execute(
        "INSERT INTO wayback.source_file
           (tal, file_date, artifact, http_status, aspa_count, router_key_count, sha256, gap_class)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (tal, file_date, artifact) DO UPDATE
           SET http_status = EXCLUDED.http_status,
               aspa_count = EXCLUDED.aspa_count,
               router_key_count = EXCLUDED.router_key_count,
               sha256 = EXCLUDED.sha256,
               gap_class = EXCLUDED.gap_class",
        &[
            &file.tal,
            &day,
            &ASPA_ARTIFACT,
            &file.http_status,
            &(file.file_ok.then_some(file.entries.len() as i32)),
            &(file.file_ok.then_some(file.quality.router_keys as i32)),
            &file.sha256,
            &gap_class,
        ],
    )?;

    if !file.file_ok {
        tx.commit().context("commit transaction")?;
        return Ok((0, 0));
    }

    let mut today: HashMap<i64, &AspaEntry> = HashMap::with_capacity(file.entries.len());
    for entry in file.entries {
        today.insert(entry.customer_asn, entry);
    }
    let current = load_current_aspa(&mut tx, file.tal, day)?;

    let mut updated = 0i64;

    // 1. Closures: a changed provider set closes its span; a customer missing
    //    from a successfully ingested day disappears.
    for (customer_asn, current_row) in &current {
        match today.get(customer_asn) {
            Some(entry) if entry.providers != current_row.providers => {
                updated += tx.execute(
                    "UPDATE wayback.aspa_version
                        SET last_seen = GREATEST(LEAST(COALESCE(last_seen, 'infinity'::date), $1), first_seen)
                      WHERE aspa_obj_id = $2
                        AND COALESCE(last_seen, 'infinity'::date) >= $1::date + 1
                        AND first_seen <= $1::date + 1",
                    &[&prev_day, &current_row.aspa_obj_id],
                )? as i64;
            }
            Some(_) => {}
            None => {
                if let Some(last_seen) =
                    last_seen_before_absence(current_row.object_first_seen, day)
                {
                    updated += tx.execute(
                        "UPDATE wayback.aspa_object SET last_seen = $1
                          WHERE aspa_obj_id = $2 AND last_seen IS NULL",
                        &[&last_seen, &current_row.aspa_obj_id],
                    )? as i64;
                }
                if let Some(last_seen) =
                    last_seen_before_absence(current_row.version_first_seen, day)
                {
                    updated += tx.execute(
                        "UPDATE wayback.aspa_version SET last_seen = $1
                          WHERE aspa_obj_id = $2 AND last_seen IS NULL",
                        &[&last_seen, &current_row.aspa_obj_id],
                    )? as i64;
                }
            }
        }
    }

    // 2. Inserts: every set not already covered on this day, via a staging COPY
    //    plus set-based INSERT (per-row round trips do not scale on a first
    //    load). The coverage guard also makes replaying a day a no-op.
    let inserted: i64;
    {
        tx.execute(
            "CREATE TEMP TABLE IF NOT EXISTS stage_aspa (
               customer bigint, providers bigint[], provider_count smallint,
               has_as0 boolean, as0_only boolean) ON COMMIT DROP",
            &[],
        )?;
        tx.execute("TRUNCATE stage_aspa", &[])?;
        {
            use std::io::Write;
            let mut writer = tx.copy_in(
                "COPY stage_aspa (customer, providers, provider_count, has_as0, as0_only) FROM STDIN",
            )?;
            let mut buffer = String::new();
            for entry in file.entries {
                let providers = entry
                    .providers
                    .iter()
                    .map(|provider| provider.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                buffer.push_str(&format!(
                    "{}\t{{{}}}\t{}\t{}\t{}\n",
                    entry.customer_asn,
                    providers,
                    entry.providers.len(),
                    entry.has_as0,
                    entry.as0_only
                ));
                if buffer.len() > 1 << 20 {
                    writer.write_all(buffer.as_bytes())?;
                    buffer.clear();
                }
            }
            writer.write_all(buffer.as_bytes())?;
            writer.finish()?;
        }
        tx.execute(
            "INSERT INTO wayback.aspa_object (ta, customer_asn, first_seen)
             SELECT $1::text, s.customer, $2::date FROM stage_aspa s
             ON CONFLICT (ta, customer_asn) DO UPDATE
               SET last_seen = NULL,
                   first_seen = LEAST(wayback.aspa_object.first_seen, EXCLUDED.first_seen)",
            &[&file.tal, &day],
        )?;
        inserted = tx.execute(
            "INSERT INTO wayback.aspa_version
               (aspa_obj_id, providers, provider_count, has_as0, as0_only, first_seen, last_seen)
             SELECT o.aspa_obj_id, s.providers, s.provider_count, s.has_as0, s.as0_only, $2::date,
                    (SELECT min(w.first_seen) - 1 FROM wayback.aspa_version w
                      WHERE w.aspa_obj_id = o.aspa_obj_id AND w.first_seen > $2::date)
             FROM stage_aspa s
             JOIN wayback.aspa_object o ON o.ta = $1 AND o.customer_asn = s.customer
             WHERE NOT EXISTS (
               SELECT 1 FROM wayback.aspa_version v
               WHERE v.aspa_obj_id = o.aspa_obj_id
                 AND v.providers = s.providers
                 AND v.first_seen <= $2::date
                 AND (v.last_seen IS NULL OR v.last_seen >= $2::date - 1))
             ON CONFLICT (aspa_obj_id, first_seen) DO NOTHING",
            &[&file.tal, &day],
        )? as i64;
    }

    tx.commit().context("commit transaction")?;
    Ok((inserted, updated))
}

/// The version that was current ON `day`: the latest span covering it. Diffing
/// against this (not the latest version overall) is what makes replaying a day
/// idempotent once later days have landed.
fn load_current_aspa(
    tx: &mut postgres::Transaction<'_>,
    tal: &str,
    day: NaiveDate,
) -> Result<HashMap<i64, CurrentAspa>> {
    let mut map = HashMap::new();
    for row in tx.query(
        "SELECT o.aspa_obj_id, o.customer_asn, o.first_seen, v.providers, v.first_seen
           FROM wayback.aspa_object o
           JOIN wayback.aspa_version v USING (aspa_obj_id)
          WHERE o.ta = $1 AND v.first_seen <= $2 AND (v.last_seen IS NULL OR v.last_seen >= $2)",
        &[&tal, &day],
    )? {
        let aspa_obj_id: i64 = row.get(0);
        let customer_asn: i64 = row.get(1);
        map.insert(
            customer_asn,
            CurrentAspa {
                aspa_obj_id,
                object_first_seen: row.get(2),
                providers: row.get(3),
                version_first_seen: row.get(4),
            },
        );
    }
    Ok(map)
}

/// Return the final observation date when an active span is absent on `day`.
/// A span that starts on `day` cannot be closed at `day - 1` without creating a
/// reverse interval.
fn last_seen_before_absence(first_seen: NaiveDate, day: NaiveDate) -> Option<NaiveDate> {
    let previous_day = day - chrono::Duration::days(1);
    (first_seen <= previous_day).then_some(previous_day)
}

/// Resume day for one TAL: the day after its latest observed ASPA file, or the
/// era start when it has never been ingested.
fn aspa_start_day(last_observed: Option<NaiveDate>) -> NaiveDate {
    last_observed
        .map(|day| day + chrono::Duration::days(1))
        .unwrap_or_else(aspa_era_start)
}

fn ingest_aspa_file(
    client: &mut Client,
    tal: &str,
    file: &ArchiveFile,
    observed_any: &mut bool,
    counts: &mut RunCounts,
) -> Result<()> {
    match parse_output_json_aspas(&file.url) {
        Ok((entries, quality)) => {
            let (inserted, updated) = ingest_aspa_day(
                client,
                &AspaFile {
                    tal,
                    day: file.file_date,
                    entries: &entries,
                    quality: &quality,
                    file_ok: true,
                    http_status: Some(200),
                    sha256: None,
                    era_start: false,
                },
            )?;
            *observed_any = true;
            counts.files_ok += 1;
            counts.rows_inserted += inserted;
            counts.rows_updated += updated;
            println!(
                "  {} objects={} rows_inserted={} rows_updated={} deviations={}",
                file.file_date,
                entries.len(),
                inserted,
                updated,
                quality.deviations()
            );
            Ok(())
        }
        Err(error) => {
            let era_start = !*observed_any;
            if era_start {
                println!(
                    "  {} era-start (ASPA artifact not published yet)",
                    file.file_date
                );
            } else {
                eprintln!("  MISSING {}: {error:#}", file.file_date);
                counts.files_failed += 1;
            }
            ingest_aspa_day(
                client,
                &AspaFile {
                    tal,
                    day: file.file_date,
                    entries: &[],
                    quality: &AspaQuality::default(),
                    file_ok: false,
                    http_status: None,
                    sha256: None,
                    era_start,
                },
            )?;
            Ok(())
        }
    }
}

fn ingest_aspa_tal_range(
    client: &mut Client,
    tal_url: &str,
    from: NaiveDate,
    until: NaiveDate,
    require_every_day: bool,
    counts: &mut RunCounts,
) -> Result<()> {
    let tal = tal_from_url(tal_url);
    println!("== TAL {tal} (aspa) ==");
    let mut files = crawl_tal_artifact(tal_url, Some(from), Some(until), ASPA_ARTIFACT);
    files.sort_by_key(|file| file.file_date);
    let mut observed_any = last_observed_day(client, tal, ASPA_ARTIFACT)?.is_some();

    if !require_every_day {
        for file in &files {
            ingest_aspa_file(client, tal, file, &mut observed_any, counts)?;
        }
        return Ok(());
    }

    let mut files_by_day = BTreeMap::new();
    for file in files {
        files_by_day.insert(file.file_date, file);
    }
    let mut day = from;
    while day <= until {
        match files_by_day.remove(&day) {
            Some(file) => ingest_aspa_file(client, tal, &file, &mut observed_any, counts)?,
            None => {
                let era_start = !observed_any;
                if era_start {
                    println!("  {day} era-start (ASPA artifact not published yet)");
                } else {
                    eprintln!("  MISSING {day}: not listed by RIPE FTP");
                    counts.files_failed += 1;
                }
                ingest_aspa_day(
                    client,
                    &AspaFile {
                        tal,
                        day,
                        entries: &[],
                        quality: &AspaQuality::default(),
                        file_ok: false,
                        http_status: None,
                        sha256: None,
                        era_start,
                    },
                )?;
            }
        }
        day += chrono::Duration::days(1);
    }
    Ok(())
}

/// Daily incremental ASPA ingest. Each TAL resumes one day after its own latest
/// observed `output.json.xz`, independently of the ROA cursor.
pub fn pg_aspa_update(config: &str, until: Option<NaiveDate>, tals: &[String]) -> Result<()> {
    let mut client = Client::connect(config, NoTls).context("connect to PostgreSQL")?;
    let until =
        until.unwrap_or_else(|| chrono::Utc::now().date_naive() - chrono::Duration::days(1));
    let tal_urls = selected_tal_urls(tals)?;

    run_ingest(&mut client, "update-aspa", |client, counts| {
        for tal_url in &tal_urls {
            let tal = tal_from_url(tal_url);
            let from = aspa_start_day(last_observed_day(client, tal, ASPA_ARTIFACT)?);
            if from > until {
                println!("== TAL {tal} (aspa) == already observed through {until}");
                continue;
            }
            ingest_aspa_tal_range(client, tal_url, from, until, true, counts)?;
        }
        Ok(())
    })
}

/// Historical ASPA ingest over an explicit range. The range is clamped to the
/// JSON era (2023-10-11); days before a TAL's first published artifact are
/// recorded as `era_start`, not as failures.
pub fn pg_aspa_backfill(
    config: &str,
    from: NaiveDate,
    until: NaiveDate,
    tals: &[String],
) -> Result<()> {
    if from > until {
        bail!("--from must not be later than --until");
    }
    let from = std::cmp::max(from, aspa_era_start());
    let mut client = Client::connect(config, NoTls).context("connect to PostgreSQL")?;
    let tal_urls = selected_tal_urls(tals)?;

    run_ingest(&mut client, "backfill-aspa", |client, counts| {
        for tal_url in &tal_urls {
            ingest_aspa_tal_range(client, tal_url, from, until, false, counts)?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_provider_sets_and_counts_deviations() {
        let raw = r#"{
            "metadata": {"generated": 1},
            "roas": [{"prefix": "1.1.1.0/24"}],
            "routerKeys": [{}, {}],
            "aspas": [
                {"customer": "AS553", "providers": ["AS2914", "AS559", "AS2914"], "ta": "ripencc"},
                {"customer": "AS1299", "providers": ["AS0"], "ta": "ripencc"},
                {"customer": "AS65000", "providers": ["AS0", "AS65001"], "ta": "ripencc"},
                {"customer": "AS65010", "providers": ["AS65010", "AS65011"], "ta": "ripencc"}
            ]
        }"#;
        let path = std::env::temp_dir().join(format!("wayback_aspa_{}.json", std::process::id()));
        std::fs::write(&path, raw).expect("write fixture");

        let (entries, quality) =
            parse_output_json_aspas(path.to_str().expect("utf-8 path")).expect("parse fixture");
        let _ = std::fs::remove_file(&path);

        assert_eq!(entries.len(), 4);
        assert_eq!(quality.router_keys, 2);

        let first = &entries[0];
        assert_eq!(first.customer_asn, 553);
        assert_eq!(first.providers, vec![559, 2914]);
        assert!(!first.has_as0);

        let as0_only = &entries[1];
        assert!(as0_only.as0_only);
        assert_eq!(as0_only.providers, vec![0]);

        // One out-of-order/duplicate provider list (AS2914 before AS559, and
        // AS2914 repeated), one AS0 next to a provider, and one customer that
        // lists itself: three deviations, all recorded, none repaired.
        assert_eq!(quality.non_ascending_or_duplicate, 1);
        assert_eq!(quality.as0_alongside_others, 1);
        assert_eq!(quality.customer_in_own_set, 1);
        assert_eq!(quality.deviations(), 3);
        assert_eq!(quality.objects, 4);
    }

    #[test]
    fn parses_plain_and_prefixed_asns() {
        assert_eq!(parse_asn("AS0").expect("AS0"), 0);
        assert_eq!(parse_asn("AS6939").expect("AS6939"), 6939);
        assert_eq!(parse_asn("4294967295").expect("max ASN"), 4294967295);
        assert!(parse_asn("AS4294967296").is_err());
        assert!(parse_asn("not-an-asn").is_err());
    }

    #[test]
    fn era_start_bounds_the_first_day() {
        assert_eq!(aspa_era_start().to_string(), "2023-10-11");
        assert_eq!(aspa_start_day(None), aspa_era_start());
        assert_eq!(
            aspa_start_day(Some(aspa_era_start())),
            aspa_era_start() + chrono::Duration::days(1)
        );
    }

    #[test]
    fn absence_closes_a_span_seen_only_on_the_previous_day() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 14).expect("valid day");
        assert_eq!(
            last_seen_before_absence(day - chrono::Duration::days(1), day),
            Some(day - chrono::Duration::days(1))
        );
        assert_eq!(last_seen_before_absence(day, day), None);
    }
}
