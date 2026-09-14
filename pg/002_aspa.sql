-- wayback-rpki ASPA schema (002): ASN-keyed SCD-2 for RIPE's `output.json.xz`.
--
-- ASPA objects carry only {customer AS, provider set}: no URI, no certificate
-- window, so the provider set is the only mutable attribute and a row is
-- written only when the set changes. History starts 2023-10-11, the day the
-- JSON artifact itself first appeared in the archive.
--
-- Object layer  : one row per (TA, customer ASN) -- the ASN-keyed identity.
-- Version layer : one row per distinct provider set, spanning the days it was
--                 observed. Unchanged days write nothing.

CREATE EXTENSION IF NOT EXISTS btree_gist;

CREATE TABLE IF NOT EXISTS wayback.aspa_object (
  aspa_obj_id  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  ta           text   NOT NULL,
  customer_asn bigint NOT NULL,
  first_seen   date   NOT NULL,
  last_seen    date,
  UNIQUE (ta, customer_asn)
);
CREATE INDEX IF NOT EXISTS aspa_object_customer_idx
  ON wayback.aspa_object (customer_asn, last_seen);

CREATE TABLE IF NOT EXISTS wayback.aspa_version (
  aspa_obj_id    bigint   NOT NULL REFERENCES wayback.aspa_object,
  providers      bigint[] NOT NULL,  -- canonical: ascending, unique, AS0 alone or absent
  provider_count smallint NOT NULL,
  has_as0        boolean  NOT NULL,
  as0_only       boolean  NOT NULL,
  first_seen     date     NOT NULL,
  last_seen      date,
  PRIMARY KEY (aspa_obj_id, first_seen)
);

-- Provider direction: containment (`providers @> ARRAY[asn]`) is the only
-- indexed access path -- an equality on an unnested (pair-grain) column cannot
-- use this index. The partial index serves the common "current posture" call.
CREATE INDEX IF NOT EXISTS aspa_version_providers_idx
  ON wayback.aspa_version USING gin (providers);
CREATE INDEX IF NOT EXISTS aspa_version_providers_current_idx
  ON wayback.aspa_version USING gin (providers) WHERE last_seen IS NULL;
CREATE INDEX IF NOT EXISTS aspa_version_span_idx
  ON wayback.aspa_version (first_seen, last_seen);

DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'aspa_version_span_excl') THEN
    ALTER TABLE wayback.aspa_version
      ADD CONSTRAINT aspa_version_span_excl EXCLUDE USING gist (
        aspa_obj_id WITH =,
        daterange(first_seen, last_seen, '[]') WITH &&
      );
  END IF;
END $$;

-- Current-state views (_view suffix per convention).
CREATE OR REPLACE VIEW wayback.aspa_object_current_view AS
  SELECT * FROM wayback.aspa_object WHERE last_seen IS NULL;
CREATE OR REPLACE VIEW wayback.aspa_version_current_view AS
  SELECT * FROM wayback.aspa_version WHERE last_seen IS NULL;

-- Raw pair grain: one row per (customer, provider) observation span. Use it for
-- set-based work (joins, distributions, exports); for a point lookup by
-- provider use wayback.aspa_customers_of(), which keeps the containment
-- predicate on wayback.aspa_version.
CREATE OR REPLACE VIEW wayback.aspa_provider_view AS
  SELECT o.ta, o.customer_asn, p.provider_asn, v.first_seen, v.last_seen
  FROM wayback.aspa_version v
  JOIN wayback.aspa_object o USING (aspa_obj_id)
  CROSS JOIN LATERAL unnest(v.providers) AS p(provider_asn);

-- Current effective posture (U-SPAS): union across publication points; AS0 is
-- removed when the union has two or more values and kept only when every
-- contributing object is AS0-only (draft-ietf-sidrops-aspa-profile-29 5.2).
CREATE OR REPLACE VIEW wayback.aspa_effective_current_view AS
WITH unioned AS (
  SELECT DISTINCT o.customer_asn, p.provider_asn
  FROM wayback.aspa_version v
  JOIN wayback.aspa_object o USING (aspa_obj_id)
  CROSS JOIN LATERAL unnest(v.providers) AS p(provider_asn)
  WHERE v.last_seen IS NULL
), grouped AS (
  SELECT customer_asn,
         array_agg(provider_asn ORDER BY provider_asn) AS providers,
         count(*) AS provider_count,
         bool_or(provider_asn = 0) AS has_as0
  FROM unioned GROUP BY customer_asn
)
SELECT customer_asn,
       CASE WHEN provider_count = 1 AND has_as0
            THEN providers
            ELSE array_remove(providers, 0) END AS providers,
       CASE WHEN provider_count = 1 AND has_as0
            THEN provider_count
            ELSE provider_count - has_as0::int END AS provider_count,
       has_as0
FROM grouped;

-- Customer direction as of a day: the AS's provider set (U-SPAS applied).
CREATE OR REPLACE FUNCTION wayback.aspa_providers_of(p_customer bigint, p_day date)
RETURNS TABLE (provider_asn bigint)
LANGUAGE sql STABLE AS $$
  WITH unioned AS (
    SELECT DISTINCT p.provider_asn
    FROM wayback.aspa_version v
    JOIN wayback.aspa_object o USING (aspa_obj_id)
    CROSS JOIN LATERAL unnest(v.providers) AS p(provider_asn)
    WHERE o.customer_asn = p_customer
      AND v.first_seen <= p_day
      AND (v.last_seen IS NULL OR v.last_seen >= p_day)
  )
  SELECT provider_asn FROM unioned
  WHERE provider_asn <> 0 OR (SELECT count(*) FROM unioned) = 1
  ORDER BY provider_asn
$$;

-- Provider direction as of a day: the customers that list this AS as a
-- provider. Containment keeps the GIN index in play; DISTINCT folds the
-- cross-TAL union.
CREATE OR REPLACE FUNCTION wayback.aspa_customers_of(p_provider bigint, p_day date)
RETURNS TABLE (customer_asn bigint)
LANGUAGE sql STABLE AS $$
  SELECT DISTINCT o.customer_asn
  FROM wayback.aspa_version v
  JOIN wayback.aspa_object o USING (aspa_obj_id)
  WHERE p_provider <> 0
    AND v.providers @> ARRAY[p_provider]
    AND v.first_seen <= p_day
    AND (v.last_seen IS NULL OR v.last_seen >= p_day)
  ORDER BY 1
$$;

-- Per-TAL per-day ASPA totals from the source ledger (mirrors roa_counts_view).
CREATE OR REPLACE VIEW wayback.aspa_counts_view AS
  SELECT tal, file_date AS day, aspa_count, gap_class
  FROM wayback.source_file
  WHERE artifact = 'output.json.xz' AND aspa_count IS NOT NULL;
