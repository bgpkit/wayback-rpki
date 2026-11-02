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
WITH effective AS (
    SELECT o.prefix AS prefix,
           o.origin_asn,
           v.max_len,
           GREATEST(
               v.first_seen::timestamp,
               COALESCE(v.not_before AT TIME ZONE 'UTC', timestamp '1970-01-01')
           ) AS lo,
           LEAST(
               COALESCE(v.last_seen, CURRENT_DATE)::timestamp + interval '1 day',
               COALESCE(v.not_after AT TIME ZONE 'UTC', timestamp '2100-01-01') + interval '1 microsecond'
           ) AS hi
      FROM wayback.roa_object o
      JOIN wayback.roa_version v ON v.roa_obj_id = o.roa_obj_id
), merged AS (
    SELECT prefix,
           origin_asn,
           max_len,
           range_agg(tsrange(lo, hi, '[)')) AS spans
      FROM effective
     WHERE lo <= hi
     GROUP BY 1, 2, 3
)
SELECT prefix::text AS prefix,
       origin_asn::bigint AS origin_asn,
       max_len::int AS max_len,
       extract(epoch FROM lower(s))::bigint AS from_ts,
       extract(epoch FROM upper(s))::bigint AS to_ts
  FROM merged, LATERAL unnest(merged.spans) AS s
 WHERE upper(s) >= (timestamp '1970-01-01' + ($1::bigint) * interval '1 second')
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
    // so the artifact is staged locally and copied out afterwards.
    let staged = std::env::temp_dir().join(format!(
        "wayback-spans-{}-{}.parquet",
        std::process::id(),
        Utc::now().timestamp()
    ));
    let file =
        std::fs::File::create(&staged).with_context(|| format!("create {}", staged.display()))?;
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(Default::default()))
        .build();
    let mut writer = ArrowWriter::try_new(file, schema(), Some(props)).context("open parquet")?;

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

    if out != staged.to_string_lossy() {
        let mut src = oneio::get_reader(&staged.to_string_lossy())
            .context("reopen staged parquet for upload")?;
        let mut dst = oneio::get_writer(out).with_context(|| format!("open {out}"))?;
        std::io::copy(&mut src, &mut dst).with_context(|| format!("copy to {out}"))?;
        dst.flush().ok();
        std::fs::remove_file(&staged).ok();
    }

    tracing::info!(
        rows = total,
        batches = flushed,
        bytes = std::fs::metadata(&staged).map(|m| m.len()).unwrap_or(0),
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
