# AGENTS.md — wayback-rpki

## Overview

Rust service that crawls historical RPKI ROA dumps from RIPE (`ftp.ripe.net/rpki/`),
stores them in a `prefix-trie` `JointPrefixMap`, and serves a REST API for querying ROA
history by prefix, ASN, max-length, date, and currency. Ships as a CLI binary and a
Docker image.

Since v2.0.0 the on-disk format is an `rkyv` archive (`roas_trie.rkyv`) that is
memory-mapped at startup — RSS drops from ~756 MB to ~5 MB and startup from ~2.3 s to
~23 ms. Bootstrap downloads and remote backups use a platform-agnostic JSONL.gz
transport. Legacy v1 `.bin`/`.bin.gz` archives (bincode + `ipnet-trie`) are still
supported via a dual-backend design and auto-convert on first run.

## Project Structure

```
wayback-rpki/
├── src/
│   ├── lib.rs          # Crawler: TAL URL discovery, ROA CSV parsing, public types
│   ├── roas_trie.rs    # v2 core: rkyv archive, JointPrefixMap storage, JSONL transport
│   ├── legacy.rs       # v1 backend: bincode + ipnet-trie, kept for transition
│   ├── api.rs          # Axum HTTP API: /search, /validate, /health; TrieBackend enum
│   └── bin/main.rs     # CLI: rebuild, update, fix, search, convert, serve
├── Dockerfile          # cargo-chef multi-stage build (rust:1.90 → debian:trixie-slim)
├── Cargo.toml          # Crate metadata; bin name = wayback-rpki
├── DEVELOPMENT.md      # Reproducible data-integrity and transport-generation operations
└── .github/workflows/
    ├── rust.yml        # Push/PR to main: cargo fmt --check + clippy -D warnings + tests
    ├── release.yml     # Tag-triggered: build-test gate → GitHub release → binaries → cargo publish
    └── docker.yml      # Main/tag/dispatch-triggered: native multi-arch Docker Hub push
```

## Key Modules

### `src/lib.rs` — Crawler & Parsers

- **`crawl_tal_after(tal_url, from, until)`** — Scrapes RIPE's year/month/day directory
  listing for a given TAL, returns `Vec<RoaFile>` metadata. Uses `rayon` for parallel crawling.
- **`parse_roas_csv(url)`** — Downloads and parses a `roas.csv.xz` file into `Vec<RoaEntry>`.
  Each entry has `tal`, `prefix` (`IpNet`), `max_len`, `asn`, `date` (`NaiveDate`).
- **`get_tal_urls(tal)`** — Returns RIPE RPKI TAL URLs for the 5 RIRs (afrinic, apnic, arin,
  lacnic, ripencc). `None` → all TALs.
- **Public types**: `RoaEntry`, `RoaFile` — re-exported at crate root along with
  `api::*` and `roas_trie::*`.

### `src/roas_trie.rs` — v2 Core Data Structure

- **`RoasTrieData`** — The serialized archive (rkyv): header (`format_version`,
  `latest_date`, `ipv4_count`, `ipv6_count`) plus `trie: JointPrefixMap<IpNet,
  Vec<RoaRecord>>`. `FORMAT_VERSION = 2` is checked on every open/load.
- **`RoaRecord`** — One archived ROA: `max_len`, `origin`, and compressed date
  `ranges: Vec<(i64, i64)>` (UTC day-granularity timestamps).
- **`RoasTrieMut`** — Mutable builder used by rebuild/update/fix/convert. Wraps
  `JointPrefixMap<IpNet, Vec<RoaRecordMut>>`; `RoaRecordMut` dual-stores
  `dates: HashSet<i64>` during bootstrap and `ranges` after compression.
  - `load(path)` — Reads a v2 archive into a mutable trie (full deserialize).
  - `dump(path)` — Compresses dates, serializes with rkyv, writes to `path.tmp` then
    atomically renames (safe hot-reload while the API is serving the old mmap).
  - `process_entries(entries, bootstrap)` / `update(tal, until)` / `fill_gaps()` —
    same semantics as v1.
- **`RoasTrie`** — Read-only query handle. `open(path)` memory-maps the archive with
  `memmap2` and validates once; queries run zero-copy on the mapped bytes.
  `from_bytes(bytes)` does the same over owned bytes.
  - `search(prefix, origin, max_len, date, current, exact)` — `exact=true` (default)
    does an exact trie `get()`; `exact=false` reproduces the old `matches()` semantics
    (shortest covering prefix + its entire subtree, i.e. supernets and subnets).
  - `validate(prefix, origin, date_ts)` — RPKI validation: `Valid` / `Invalid` / `Unknown`.
  - `lookup_prefix(prefix)` — All ROAs matching a prefix, including supernets/subnets.
- **JSONL transport** — `JsonlRecord { p, m, o, r }`, one record per line. Platform-agnostic
  and streamable; used for bootstrap downloads and remote backups (the `.rkyv` archive
  itself is platform-specific).
  - `RoasTrieMut::import_jsonl(path)` / `from_jsonl(path)` — Streams one record at a
    time; validates input and reports physical line numbers on error. Peak memory is
    just the builder (~200 MB for the full dataset).
  - `RoasTrie::export_jsonl(path)` — Streams zero-copy from the mmap archive.
- **`REMOTE_BOOTSTRAP_URL`** — `https://spaces.bgpkit.org/broker/roas_trie.jsonl.gz`.

### `src/legacy.rs` — v1 Backend (transition only)

- **`LegacyRoasTrie`** — The v1 in-memory implementation: `IpnetTrie<HashMap<(u8, u32),
  LegacyRoasTrieEntry>>` serialized with bincode (via `oneio`, `.gz`-aware). Implements
  the same query surface (`search`, `validate`, `lookup_prefix`, `counts` via iteration,
  `update`, `fill_gaps`).
- **`to_builder()`** — Converts a loaded legacy trie into a `RoasTrieMut` for writing
  a v2 archive. Used by `convert` and by automatic first-run migration.
- The v1 deps (`ipnet-trie`, `bincode`) are always compiled in during the transition
  period. Note: `bincode` 2.x is flagged unmaintained by `cargo audit` — acceptable
  while legacy support is retained.

### `src/api.rs` — HTTP API (Axum)

- **`TrieBackend`** — `V2(RoasTrie)` | `V1(LegacyRoasTrie)`; uniform
  `search`/`validate`/`counts`/`latest_date_ts`/`format_version` surface.
  `format_version()` returns 2 for rkyv, 1 for legacy.
- **`/search`** — Params: `asn`, `prefix`, `max_len`, `date`, `current`, `page`,
  `page_size` (max 1000), `exact` (default `true`). Paginated JSON; `meta` includes
  `latest_date` and `format_version`. Malformed `prefix`/`date` → `400` JSON error.
- **`/validate`** — Params: `prefix` (required), `asn` (required), `date` (default:
  latest). Returns `valid` / `invalid` / `unknown`.
- **`/health`** — IPv4/IPv6 ROA counts, `latest_date`, `format_version`.
- **`start_api_service(trie, host, port, root)`** — Axum server with CORS;
  `root != "/"` nests routes under a path prefix.

### `src/bin/main.rs` — CLI

- **Global args**: `path` (default `roas_trie.rkyv`; suffix selects the backend:
  `.rkyv` → v2 mmap, `.bin`/`.bin.gz` → v1 legacy in-memory), `--bootstrap`,
  `--env` (dotenv path).
- **Subcommands**:
  - `rebuild` — Full historical rebuild (`--tal`, `--chunks`, `--from`, `--until`);
    parallel parse + progress bar, single writer thread.
  - `update` — Incremental update from `latest_date + 1` (`--tal`, `--until`).
  - `fix <input.jsonl[.gz]> [-o <output.jsonl[.gz]>]` — Apply the known-gap
    policy to a portable JSONL transport. Without `-o/--output`, overwrites the
    input in place via atomic rename; see `DEVELOPMENT.md`.
  - `search` — CLI search (`--asn`, `--prefix`, `--max_len`, `--date`, `--current`,
    `--exact`); markdown table output.
  - `convert --from <legacy.bin.gz>` — Convert a v1 archive to the v2 rkyv format.
  - `serve` — API server: `--host` (default `0.0.0.0`), `--port` (default `40065`),
    `--backup-to`, `--update-interval` (seconds, default `28800` = 8 h). A background
    thread reloads the trie from disk, dumps it atomically, writes backups, then swaps
    the serving `TrieBackend` behind an `RwLock`.

## Bootstrap & Migration

`check_bootstrap_and_download` precedence when the requested `.rkyv` is missing:

1. **Local sibling** `roas_trie.bin.gz` / `roas_trie.bin` → auto-convert to `.rkyv`.
2. **Remote JSONL** `roas_trie.jsonl.gz` (`--bootstrap`) → stream-import → dump `.rkyv`.
3. **Remote legacy** `roas_trie.bin.gz` (`--bootstrap`, fallback) → download → auto-convert.

A `.bin`/`.bin.gz` `path` keeps working directly in legacy mode without conversion.

## Architecture Notes

- **Data flow**: RIPE CSV dumps → `parse_roas_csv` → `RoasTrieMut.process_entries` →
  rkyv-serialized `RoasTrieData` (`.rkyv`) → memory-mapped by `RoasTrie` for serving.
- **Backups**: after each background update cycle, `backup_archive` streams the mmap
  archive to `.jsonl.gz` and uploads to S3/R2 (`s3://`/`r2://` via `oneio::s3_upload`,
  requires `AWS_*` env vars) or writes locally. Destinations: `--backup-to` plus
  `WAYBACK_BACKUP_TO` env var. `WAYBACK_BACKUP_HEARTBEAT_URL` is GET-pinged after a
  successful backup.
- **Production deployment**: Docker Compose on `bh-01`/`bh-02` (Tailscale), proxied by
  Cloudflare Worker at `alpha.api.bgpkit.com` with failover. Container entrypoint:
  `wayback-rpki serve --bootstrap --host 0.0.0.0 --port 40065`.

## Build & Test

```bash
cargo build                              # Build
cargo clippy --all-features -- -D warnings  # Lint (CI-enforced)
cargo test --lib roas_trie::tests         # Offline unit tests (plus src/bin/main.rs tests)
cargo test                                # All tests (lib.rs crawler tests need network to ftp.ripe.net)
```

## CI/CD

CI mirrors the monocle repo's workflow layout.

- **`rust.yml`** — On push/PR to main (`**.md` ignored): `cargo fmt --check`,
  `cargo clippy --all-features -- -D warnings`, `cargo test --all-features --verbose`.
- **`release.yml`** — On `v*` tag: `build-test` gate (fmt, clippy, build, test) →
  GitHub release (from `CHANGELOG.md`) → binary uploads for `aarch64-linux`,
  `x86_64-linux`, `universal-apple-darwin` (`macos-14` runner) → `cargo publish`
  runs last, only after binaries are uploaded.
- **`docker.yml`** — One workflow for all image builds. On push to main (filtered to
  `Dockerfile`, `src/**`, Cargo manifests, `.dockerignore`): pushes
  `bgpkit/wayback-rpki:main`. On `v*` tag: pushes semver tags + `latest`. Also
  supports `workflow_dispatch` with a version input for manual multi-tag rebuilds.
  Builds `linux/amd64` + `linux/arm64` natively (`ubuntu-latest` / `ubuntu-24.04-arm`
  matrix, no QEMU), pushes per-arch images by digest with registry cache, then merges
  into multi-arch manifests on Docker Hub.

## Conventions

- Clippy must pass with `-D warnings` — CI enforces this.
- `sort_by_key` is preferred over `sort_by(|a, b| ...)` (newer clippy lint).
- Type aliases are used for complex generics (see `RoasTrieMap` in `legacy.rs`) to
  satisfy `clippy::type_complexity`.
- Exact-match semantics: `/search?prefix=` and CLI `search` default to exact prefix
  match (`exact=true`); `exact=false` includes supernets and subnets (the pre-v1.0.5
  `matches()` behavior that caused issue #9). Keep `exact=true` as the default for
  prefix lookups unless inclusive matching is explicitly requested.
- The `.rkyv` archive is platform-specific (endianness/layout) — never distribute it
  across machines; use the JSONL.gz transport for bootstrap/backup exchange.
- v2 search results are deterministically ordered (trie lexicographic prefix order,
  then `(origin, max_len)` within a prefix); v1 legacy order is nondeterministic
  HashMap order.

## Known Gaps and Data Repair

`KNOWN_GAPS_STR` in `roas_trie.rs` defines an interpolation policy; the `fix`
subcommand fills listed intervals only where an identical record exists on both
boundaries. A range discontinuity may be a product-history omission, a raw-
source discontinuity, or both. Do not label interpolated data as observed.

Read `DEVELOPMENT.md` before investigating a gap, modifying `KNOWN_GAPS_STR`,
or generating a replacement JSONL transport. It contains the D-1/D/D+1 source
comparison, provenance requirements, helper command, and required validation.
