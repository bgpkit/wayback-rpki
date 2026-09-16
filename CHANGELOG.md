# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

### Fixed (wayback-pg ingest correctness, from review of #19)

* Applying a day out of order no longer drops the observation: a span that ends the day before is extended, and an absent day in the middle of a span is split so days observed after it keep their own span.
* `roa_object.last_seen` and `aspa_object.last_seen` are derived from the version spans instead of being written by the day being applied, so replaying an older day neither reopens nor closes an object that later history still covers.
* ASPA `era_start` requires the archive's own listing as evidence; a fetch or parse failure now fails the run instead of fabricating an era start.
* Backfills record every calendar day they did not ingest as a `source_file` gap instead of skipping it, and a listing that cannot be read fails the run instead of reporting success with no work.
* `roa_counts_view` no longer reports a missing file as an observed zero count, and an unknown `--tal` name is a CLI error instead of a panic.
* `roa_tuple_view` returns one row per contiguous span, so a tuple that disappeared and returned is two rows rather than one range with the gap filled in.

* A repaired day whose attributes changed (ROA) or whose provider set changed (ASPA) now keeps the days after it under the previous values and replaces that day's own row, instead of writing the new values over later observations or dropping the change on a primary-key clash.
* `roa_tuple_view` folds the spans of a tuple into contiguous rows, so several objects authorizing the same tuple no longer fragment or overlap one span.
* The ROA ingest loads only the TAL being applied, so another TAL's objects can neither be closed nor marked by it.
* ASPA `era_start` also requires the walk to have begun at the archive's ASPA era; a range that starts mid-history records absent days as gaps.
* `next_update_day` points at `wayback-pg backfill`, and the ROA/ASPA storage comments state the invariants the tables actually hold.

* An incremental run stops at the first day it cannot observe, so the cursor stays before that day and the next run retries it instead of jumping past it to the latest success.
* A repair that finds the object absent on the very day its span started drops that row (keeping a verified later tail), and an object left with no observations is removed rather than left current with an empty history.
* The ASPA parser requires `aspas` and each object's `providers`: a renamed field or an error document fails the day instead of parsing as an empty snapshot that withdraws every customer.

* A span written for a repaired (out-of-order) day covers that day only, and the ROA attribute-change repair follows the same rule: a later span, or days the archive observed as absent, are no longer filled in with presence or attributes no file ever showed. The span stays open only when the repaired day is the latest day the TAL observed.
* The legacy `crawl_tal_after` / `crawl_tal_artifact` keep their best-effort traversal: a listing below the TAL root that cannot be fetched omits only that subtree, while the ingest uses the strict forms that fail the run.

* A listing that comes back readable but without a single entry is treated as a broken response rather than an empty archive, so the strict crawl cannot turn an error page into a run that records every day as a gap and succeeds.
* When one certificate authorizes the same prefix and origin at more than one max length in a snapshot, the longest authorization is stored: it is the one that validates traffic, and the previous smallest-wins rule understated it.

* `roa_tuple_view` merges the version spans per tuple with `range_agg` instead of expanding each span into one calendar-day row: on 200k spans of 30 days the old form materialized 6.0M intermediate rows and spilled ~200 MB to disk per scan, the new one works on 200k spans, and both return identical rows.
* A snapshot that lists the same ASPA customer twice now loads: the day is staged from the deduplicated set, so one statement never touches the same conflict row twice.

* A walk decides whether an unavailable ASPA day is an era start from the observations at or before its starting position, not from the latest observation in the database, so backfilling early history into a database that already holds later days records those days as `era_start` instead of failing them as gaps.

* A correction that restores the attributes (ROA) or provider set (ASPA) a previous span already carries drops the day's own row instead of leaving two rows that both claim the day, which mid-statement violated the ASPA span exclusion constraint and aborted the whole day.
* An unchanged ASPA day no longer rewrites its object rows: the first-seen upsert fires only when the observation actually moves earlier.

* The `source_file` ledger only ever gains evidence: a failed replay of a day that was already observed records the run failure but keeps the `observed` row and its counts instead of downgrading the day to `missing` and clearing its provenance.
* An ASPA artifact that exists but does not parse marks publication as begun, so a backfill that continues past the failure records the days after it as gaps instead of era starts.

* Repairing the day before an existing span merges the identical span that starts the next day into one span instead of leaving two adjacent rows, which a later replay of that day would have extended over, violating the span exclusion constraint and aborting the day.
* The predecessor extension refuses to run when a span with the same attributes already covers the day, which makes such a replay a no-op instead of an overlapping update.

* The per-day reconciliation recomputes an object's `first_seen` along with its current-state marker, so a repair that removes the earliest version but keeps a later one no longer leaves the object pointing at a day no version covers.
* `ingest_run` has an `error` column: a run that fails before or outside file accounting (an unreadable listing, a database error) is no longer indistinguishable from a successful no-op run.

### Added

* `wayback-pg`: a second binary that ingests RIPE RPKI observations into PostgreSQL and leaves the v1 trie, HTTP API, and `wayback-rpki` CLI untouched. `update` and `backfill --pg-config` accept `--tal` and `--types roa,aspa` (default: both), each family resuming from its own source-file cursor.
* ASPA support in that binary: `pg/002_aspa.sql` adds ASN-keyed `aspa_object` / `aspa_version` tables that write a row only when a customer's provider set changes, plus `aspa_providers_of(customer_asn, day)` and `aspa_customers_of(provider_asn, day)` for either query direction as of a date. Cross-TAL unioning and the U-SPAS AS0 rule live in the views and functions. Coverage starts `2023-10-11`, the first day RIPE's `output.json.xz` artifact exists.
* `pg/001_schema.sql` carries the ROA SCD-2 store, the per-file `source_file` ledger, and the `ingest_run` accounting that the binary writes.
* `src/lib.rs` gains `crawl_tal_artifact()` for archive artifacts other than `roas.csv.xz`; no v1 function changed.

## v1.1.0 - 2026-07-25

### Highlights

* Zero-copy mmap serving: the ROA trie is now a `prefix-trie` `JointPrefixMap` serialized as
  an `rkyv` archive (default `roas_trie.rkyv`) and memory-mapped at startup — RSS drops from
  ~756 MB to ~5 MB and startup from ~2.3 s to ~23 ms (#12)
* Platform-agnostic JSONL.gz transport: bootstrap and backups now stream a portable
  `.jsonl.gz` format instead of platform-specific `.rkyv.gz`, cutting bootstrap peak RAM from
  ~1 GB to ~200 MB (#13)
* jemalloc global allocator: RSS drops from ~1.28 GB to ~569 MB steady-state by returning
  freed pages to the OS after bootstrap/update spikes — fits Railway's 1 GB plan (#14)
* Smooth v1 → v2 transition: legacy `.bin`/`.bin.gz` archives (local sibling or remote
  bootstrap) are auto-converted on first run — no manual migration or prompts (#12)
* Repurposed `fix` subcommand: operates on portable JSONL transports with optional
  in-place repair via atomic rename; see `DEVELOPMENT.md` for the gap-investigation
  and transport-generation procedure

### Breaking Changes

* Default data file is now `roas_trie.rkyv` (was `roas_trie.bin.gz`); `.bin`/`.bin.gz`
  paths still work in legacy in-memory mode (#12)
* Bootstrap precedence is now: local sibling `.bin.gz`/`.bin` → auto-convert, then remote
  `roas_trie.jsonl.gz` (stream-import, preferred), then remote `roas_trie.bin.gz` (legacy
  fallback) (#13)
* The `.rkyv.gz` transport format is removed — bootstrap and backups use `.jsonl.gz` (#13)

### Features

* Dual-mode trie backend: `.rkyv` paths use the v2 mmap backend, `.bin`/`.bin.gz` paths use
  the v1 legacy in-memory backend; `/health` now reports `format_version` (#12)
* New `convert` subcommand: `wayback-rpki convert --from legacy.bin.gz` writes a v2 rkyv
  archive (#12)
* Streaming JSONL import/export: `RoasTrieMut::import_jsonl` builds from a `.jsonl[.gz]`
  stream one record at a time; `RoasTrie::export_jsonl` streams zero-copy from the mmap
  archive (#13)
* Archive updates write to a temp file and atomically rename for safe hot-reload during
  background updates (#12)
* jemalloc global allocator (`tikv_jemallocator`) returns freed pages to the OS after
  bootstrap/update, cutting steady-state RSS by 55% (#14)
* `fix <input.jsonl[.gz]> [-o <output.jsonl[.gz]>]` — applies `KNOWN_GAPS_STR` to a
  portable JSONL transport; without `-o`, overwrites in place via atomic rename
* `DEVELOPMENT.md` documents the historical-gap investigation and transport-generation
  procedure

### Bug Fixes

* Validate JSONL transport input and report physical line numbers in diagnostics (#13)

### Build & CI

* Native per-platform Docker builds with cargo-chef and registry cache (#11)
* Dockerfile runtime updated to `debian:trixie-slim` (the `rust:1.90` base now ships
  Debian 13 / GLIBC 2.41) (#12)

## v1.0.5 - 2026-07-09

### Highlights

* Fix incorrect prefix search results: `/search?prefix=` now returns only exact prefix matches by default (issue #9)
* Add Docker CI workflow for automatic multi-arch image publishing to Docker Hub on tag
* Add input validation: malformed prefix or date query params now return `400` instead of `500`
* Add `AGENTS.md` and expanded `README.md` with full API and library documentation

### Bug Fixes

* **Issue #9**: `search()` used `ipnet-trie`'s `matches()` which returns all children of the
  shortest matching supernet, causing unrelated super- and sub-prefix ROAs to appear in
  results. Now defaults to `exact_match()` with a new `exact` parameter (`?exact=false`
  preserves the old inclusive behavior).
* Return `400 Bad Request` with JSON error for malformed `prefix` or `date` query
  parameters instead of panicking with a `500 Internal Server Error`.
* Resolve `clippy::unnecessary_sort_by` and `clippy::type_complexity` lints for
  compatibility with clippy 1.96+.

### Features

* Add `exact` query parameter to `/search` API (default `true`) and `--exact` CLI flag.
* Add `.github/workflows/docker.yml`: builds `linux/amd64` + `linux/arm64` images and
  pushes to `bgpkit/wayback-rpki` on Docker Hub on `v*` tags and `main` pushes.
* Add 4 offline unit tests for `search()` exact vs non-exact behavior.
* Update `Dockerfile` to Rust 1.90 and include `Cargo.lock` for reproducible builds.

## v1.0.4 - 2025-09-14

- Update dependencies
- Improve error handling for remote IO failures when crawling RIPE directory listings: replace unwraps on oneio::
  read_to_string with logging and graceful fallback to empty results.
- Ensure Serve command's background updater thread does not crash on temporary errors: handle update errors without
  panicking, log them, and continue the loop.

## v1.0.1 - 2025-03-25

### Highlights

* Add `--host` and `--port` arguments for the `wayback-rpki serve` command
* Add `/wayback-rpki` as the workdir for the Docker container
* Specify default `--host 0.0.0.0 --port 40065` for Docker container

## v1.0.0 - 2025-03-25

Stable release version.

### Features

* In-memory prefix-trie-powered data structure
* One-command bootstrap, API, backup, heartbeat with `wayback-rpki serve --bootstrap`
* Docker-deployment available

## v0.1.0 - 2024-05-08

Version v0.1.0 uses a PostgreSQL database to store all RPKI ROAS information. This version requires a Postgres setup to
work.

The future versions will be database-free.