-- wayback-rpki v2.1 PoC schema (object-grain, SCD-2)
-- Two time axes: not_before/not_after = certificate claims (from CSV);
-- first_seen/last_seen = our observation interval (NULL last_seen = current).

CREATE SCHEMA IF NOT EXISTS wayback;

-- Object layer: one row per (TA, certificate URI, prefix, origin ASN).
CREATE TABLE IF NOT EXISTS wayback.roa_object (
  roa_obj_id  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  prefix      cidr     NOT NULL,
  origin_asn  bigint   NOT NULL,
  ta          text     NOT NULL,
  uri         text     NOT NULL,
  first_seen  date     NOT NULL,
  last_seen   date,
  UNIQUE (ta, uri, prefix, origin_asn)
);
CREATE INDEX IF NOT EXISTS roa_object_prefix_idx ON wayback.roa_object USING gist (prefix inet_ops);
CREATE INDEX IF NOT EXISTS roa_object_origin_idx ON wayback.roa_object (origin_asn, last_seen);

-- Version layer: one row per attribute combination (max_len, cert window).
CREATE TABLE IF NOT EXISTS wayback.roa_version (
  roa_obj_id  bigint      NOT NULL REFERENCES wayback.roa_object,
  max_len     smallint    NOT NULL,
  not_before  timestamptz NOT NULL,
  not_after   timestamptz NOT NULL,
  first_seen  date        NOT NULL,
  last_seen   date,
  PRIMARY KEY (roa_obj_id, first_seen)
);
-- No two versions of one object may have overlapping observation spans.
DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'roa_version_span_excl') THEN
    ALTER TABLE wayback.roa_version
      ADD CONSTRAINT roa_version_span_excl EXCLUDE USING gist (
        roa_obj_id WITH =,
        daterange(first_seen, last_seen, '[]') WITH &&
      );
  END IF;
END $$;
CREATE INDEX IF NOT EXISTS roa_version_maxlen_idx ON wayback.roa_version (max_len);

-- Completeness ledger: one row per (tal, date, artifact) actually attempted.
CREATE TABLE IF NOT EXISTS wayback.source_file (
  tal          text   NOT NULL,
  file_date    date   NOT NULL,
  artifact     text   NOT NULL,     -- roas.csv.xz | output.json.xz
  http_status  smallint,
  roa_count    integer,
  aspa_count   integer,
  router_key_count integer,
  sha256       text,
  fetched_at   timestamptz NOT NULL DEFAULT now(),
  gap_class    text   NOT NULL DEFAULT 'observed',  -- observed|source_gap|missing|interpolated
  PRIMARY KEY (tal, file_date, artifact)
);

-- Ingest provenance: one row per crawler run.
CREATE TABLE IF NOT EXISTS wayback.ingest_run (
  run_id       bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  started_at   timestamptz NOT NULL,
  finished_at  timestamptz,
  mode         text NOT NULL,       -- backfill|incremental
  code_version text NOT NULL,
  files_ok     integer,
  files_failed integer,
  rows_inserted bigint,
  rows_updated  bigint
);

-- Current-state views (_view suffix per convention).
CREATE OR REPLACE VIEW wayback.roa_object_current_view AS
  SELECT * FROM wayback.roa_object WHERE last_seen IS NULL;
CREATE OR REPLACE VIEW wayback.roa_version_current_view AS
  SELECT * FROM wayback.roa_version WHERE last_seen IS NULL;

-- Tuple-grain compatibility view: folds object+version spans to
-- (prefix, origin_asn, max_len) maximal spans — trie semantics.
CREATE OR REPLACE VIEW wayback.roa_tuple_view AS
WITH covered AS (
  SELECT o.prefix, o.origin_asn, v.max_len,
         generate_series(v.first_seen, COALESCE(v.last_seen, CURRENT_DATE), interval '1 day')::date AS d
  FROM wayback.roa_object o
  JOIN wayback.roa_version v USING (roa_obj_id)
)
SELECT prefix, origin_asn, max_len, min(d) AS first_seen,
       CASE WHEN max(d) = CURRENT_DATE THEN NULL ELSE max(d) END AS last_seen
FROM covered
GROUP BY prefix, origin_asn, max_len;

-- Change events: attribute changes and lifecycle boundaries.
-- Noise policy (storage stays raw; only this view filters):
--   * short-lived versions (span <= 3 days) are excluded by default -- they are
--     publication-window oscillation (e.g. APNIC 22% of objects, LACNIC re-signs),
--     not authorization changes; pass raw=true / drop the filter for audits.
--   * objects flipping >= 3 versions inside 7 days are classified as
--     publication_flap (publisher health signal) instead of cert_window_changed.
CREATE OR REPLACE VIEW wayback.roa_change_event_view AS
SELECT
  nv.roa_obj_id,
  o.prefix, o.origin_asn, o.ta, o.uri,
  nv.max_len AS new_max_len, ov.max_len AS old_max_len,
  nv.not_before AS new_not_before, nv.not_after AS new_not_after,
  ov.not_before AS old_not_before, ov.not_after AS old_not_after,
  nv.first_seen AS event_date,
  CASE
    WHEN ov.roa_obj_id IS NULL THEN 'appeared'
    WHEN nv.max_len <> ov.max_len THEN 'maxlen_changed'
    WHEN (SELECT count(*) FROM wayback.roa_version f
          WHERE f.roa_obj_id = nv.roa_obj_id
            AND f.first_seen >= nv.first_seen - 7
            AND f.first_seen <= nv.first_seen) >= 3
      THEN 'publication_flap'
    ELSE 'cert_window_changed'
  END AS change_type,
  COALESCE(nv.last_seen, CURRENT_DATE) - nv.first_seen + 1 AS span_days
FROM wayback.roa_version nv
JOIN wayback.roa_object o USING (roa_obj_id)
LEFT JOIN wayback.roa_version ov
  ON ov.roa_obj_id = nv.roa_obj_id
 AND ov.last_seen = nv.first_seen - 1
WHERE (ov.roa_obj_id IS NULL OR nv.max_len <> ov.max_len
    OR nv.not_before <> ov.not_before OR nv.not_after <> ov.not_after)
  -- noise floor: drop events whose PREVIOUS span was a short-lived flip;
  -- real renewals have long preceding spans. appeared/maxlen always kept.
  AND (ov.roa_obj_id IS NULL OR nv.max_len <> ov.max_len
       OR COALESCE(ov.last_seen, CURRENT_DATE) - ov.first_seen + 1 > 3);
