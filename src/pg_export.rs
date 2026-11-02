//! Export the ROA effective-interval table.
//!
//! One row per (prefix, origin, max_len) interval during which the archive shows
//! that authorization in effect. An interval is the certificate window of a ROA
//! version intersected with the span over which the archive observed it, merged
//! across versions of the same object, so a consumer can answer "was this prefix
//! authorized on day D" without reading PostgreSQL at query time.
//!
//! The merge happens in SQL because the raw material is one row per observed
//! version (tens of millions) while the merged intervals are an order of
//! magnitude fewer.

use anyhow::{Context, Result};
use arrow::array::{Int64Array, StringArray, UInt32Array, UInt8Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use chrono::{NaiveDate, TimeZone, Utc};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use postgres::fallible_iterator::FallibleIterator;
use postgres::{Client, NoTls};
use std::io::Write;
use std::sync::Arc;

const BATCH_ROWS: usize = 50_000;

const EXPORT_SQL: &str = r#"
WITH observed AS (
    -- The days the archive actually saw a ROA file for a TAL. Version spans
    -- bridge days that were never fetched, and an export that kept the bridge
    -- would assert an authorization the archive never measured.
    SELECT tal, range_agg(daterange(file_date, file_date + 1, '[)')) AS days
      FROM wayback.source_file
     WHERE artifact = 'roas.csv.xz' AND gap_class = 'observed'
     GROUP BY tal
), version_span AS (
    SELECT ta, prefix, origin_asn, max_len, lo, hi
      FROM (
          SELECT o.ta,
                 o.prefix,
                 o.origin_asn,
                 v.max_len,
                 GREATEST(
                     v.first_seen::timestamp,
                     COALESCE(v.not_before AT TIME ZONE 'UTC', timestamp '1970-01-01')
                 ) AS lo,
                 LEAST(
                     (COALESCE(v.last_seen, (now() AT TIME ZONE 'UTC')::date) + 1)::timestamp,
                     COALESCE(v.not_after AT TIME ZONE 'UTC', timestamp '2100-01-01')
                 ) AS hi
            FROM wayback.roa_object o
            JOIN wayback.roa_version v ON v.roa_obj_id = o.roa_obj_id
      ) span
     WHERE lo < hi
), cut AS (
    SELECT s.prefix,
           s.origin_asn,
           s.max_len,
           tsrange(
               GREATEST(s.lo, lower(d)::timestamp),
               LEAST(s.hi, upper(d)::timestamp),
               '[)'
           ) AS piece
      FROM version_span s
      JOIN observed o ON o.tal = s.ta
      CROSS JOIN LATERAL unnest(o.days) AS d
     WHERE s.lo < upper(d)::timestamp AND s.hi > lower(d)::timestamp
), merged AS (
    SELECT prefix, origin_asn, max_len, range_agg(piece) AS spans
      FROM cut
     GROUP BY 1, 2, 3
)
SELECT prefix::text AS prefix,
       origin_asn::bigint AS origin_asn,
       max_len::int AS max_len,
       extract(epoch FROM lower(s))::bigint AS from_ts,
       -- Upper bounds are exclusive; the table carries the last inclusive second.
       extract(epoch FROM upper(s))::bigint - 1 AS to_ts
  FROM merged, LATERAL unnest(merged.spans) AS s
 WHERE upper(s) > (timestamp '1970-01-01' + ($1::bigint) * interval '1 second')
"#;

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("prefix", DataType::Utf8, false),
        Field::new("origin_asn", DataType::UInt32, false),
        Field::new("max_len", DataType::UInt8, false),
        Field::new("from_ts", DataType::Int64, false),
        Field::new("to_ts", DataType::Int64, false),
    ]))
}

/// A staging file that removes itself on drop unless it is forgotten.
struct StagedFile {
    path: std::path::PathBuf,
    file: std::fs::File,
}

impl StagedFile {
    fn as_file(&self) -> &std::fs::File {
        &self.file
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Create a staging file that cannot collide with, or be redirected by, an
/// existing path.
fn create_staging_file() -> Result<StagedFile> {
    let dir = std::env::temp_dir();
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default();
    for attempt in 0..8 {
        let path = dir.join(format!(
            "wayback-spans-{}-{nanos}-{attempt}.parquet",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok(StagedFile { path, file }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", path.display()));
            }
        }
    }
    anyhow::bail!("could not create a staging file in {}", dir.display())
}

/// Write the interval table to `out`, which may be a local path or an S3 URI.
pub fn export_spans(pg_config: &str, out: &str, since: Option<NaiveDate>) -> Result<()> {
    let since_ts = since
        .map(|d| {
            Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
                .timestamp()
        })
        .unwrap_or(0);

    let mut client = Client::connect(pg_config, NoTls).context("connect to PostgreSQL")?;
    let mut rows = client
        .query_raw(EXPORT_SQL, [&since_ts])
        .context("run span export query")?;

    // Parquet writers want `Write + Send`, and oneio hands back `Box<dyn Write>`,
    // so the artifact is staged locally and copied out afterwards. The staging file
    // is created exclusively: a predictable name opened with `create` would follow
    // a planted symlink and truncate whatever it points at.
    let staged = create_staging_file()?;
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(Default::default()))
        .build();
    let mut writer = ArrowWriter::try_new(staged.as_file().try_clone()?, schema(), Some(props))
        .context("open parquet")?;

    let mut batch = Vec::with_capacity(BATCH_ROWS);
    let mut total: u64 = 0;
    let mut flushed = 0u64;
    while let Some(row) = rows.next().context("read export row")? {
        let prefix: String = row.get(0);
        let origin: i64 = row.get(1);
        let max_len: i32 = row.get(2);
        let from_ts: i64 = row.get(3);
        let to_ts: i64 = row.get(4);
        batch.push((prefix, origin as u32, max_len as u8, from_ts, to_ts));
        if batch.len() >= BATCH_ROWS {
            write_batch(&mut writer, &batch).context("write batch")?;
            total += batch.len() as u64;
            flushed += 1;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        write_batch(&mut writer, &batch).context("write final batch")?;
        total += batch.len() as u64;
        flushed += 1;
    }
    writer.close().context("close parquet")?;
    drop(rows);
    drop(client);

    let staged_path = staged.path().to_string_lossy().to_string();
    if staged_path == out {
        // The caller asked for the staging path itself; nothing to copy.
        std::mem::forget(staged);
    } else if out.contains("://") {
        let mut src = oneio::get_reader(&staged_path).context("reopen staged parquet")?;
        let mut dst = oneio::get_writer(out).with_context(|| format!("open {out}"))?;
        std::io::copy(&mut src, &mut dst).with_context(|| format!("copy to {out}"))?;
        dst.flush().with_context(|| format!("flush {out}"))?;
    } else {
        // A local destination is replaced atomically. Opening it directly would
        // truncate the last good export before the copy finished, so a failed run
        // would leave a corrupt file where a complete one used to be.
        let dest = std::path::Path::new(out);
        let tmp = dest.with_extension(format!("tmp-{}", std::process::id()));
        {
            let mut src = oneio::get_reader(&staged_path).context("reopen staged parquet")?;
            let mut dst =
                std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            std::io::copy(&mut src, &mut dst)
                .with_context(|| format!("write {}", tmp.display()))?;
            dst.sync_all()
                .with_context(|| format!("flush {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, dest)
            .with_context(|| format!("rename {} to {}", tmp.display(), dest.display()))?;
    }

    tracing::info!(
        rows = total,
        batches = flushed,
        bytes = std::fs::metadata(&staged_path)
            .map(|m| m.len())
            .unwrap_or(0),
        out,
        "exported ROA effective intervals"
    );
    Ok(())
}

fn write_batch<W: Write + Send>(
    writer: &mut ArrowWriter<W>,
    batch: &[(String, u32, u8, i64, i64)],
) -> Result<()> {
    let prefixes: StringArray = batch.iter().map(|r| Some(r.0.as_str())).collect();
    let origins: UInt32Array = batch.iter().map(|r| Some(r.1)).collect();
    let max_lens: UInt8Array = batch.iter().map(|r| Some(r.2)).collect();
    let from: Int64Array = batch.iter().map(|r| Some(r.3)).collect();
    let to: Int64Array = batch.iter().map(|r| Some(r.4)).collect();
    let record = RecordBatch::try_new(
        schema(),
        vec![
            Arc::new(prefixes),
            Arc::new(origins),
            Arc::new(max_lens),
            Arc::new(from),
            Arc::new(to),
        ],
    )?;
    writer.write(&record)?;
    Ok(())
}
