# AGENTS.md — wayback-rpki

## Overview

Rust service that crawls historical RPKI ROA dumps from RIPE (`ftp.ripe.net/rpki/`),
stores them in an in-memory `IpnetTrie`, and serves a REST API for querying ROA
history by prefix, ASN, max-length, date, and currency. Ships as a CLI binary
and a Docker image.

## Project Structure

```
wayback-rpki/
├── src/
│   ├── lib.rs          # Crawler: TAL URL discovery, ROA CSV parsing, public types
│   ├── roas_trie.rs    # Core data structure: IpnetTrie-backed ROA storage + search
│   ├── api.rs          # Axum HTTP API: /search and /health endpoints
│   └── bin/main.rs     # CLI entry point: rebuild, update, search, fix, serve subcommands
├── Dockerfile          # Multi-stage build (rust:1.90 → debian:bookworm-slim)
├── Cargo.toml          # Crate metadata; bin name = wayback-rpki
└── .github/workflows/
    ├── rust.yml        # PR CI: cargo build + clippy -D warnings
    ├── release.yml     # Tag-triggered: GitHub release + cargo publish + binary uploads
    └── docker.yml      # Tag/main-triggered: multi-arch Docker Hub push
```

## Key Modules

### `src/lib.rs` — Crawler & Parsers

- **`crawl_tal_after(tal_url, from, until)`** — Scrapes RIPE's year/month/day directory
  listing for a given TAL, returns `Vec<RoaFile>` metadata. Uses `rayon` for parallel crawling.
- **`parse_roas_csv(url)`** — Downloads and parses a `roas.csv.xz` file into `Vec<RoaEntry>`.
  Each entry has `tal`, `prefix` (`IpNet`), `max_len`, `asn`, `date` (`NaiveDate`).
- **`get_tal_urls(tal)`** — Returns RIPE RPKI TAL URLs for the 5 RIRs (afrinic, apnic, arin,
  lacnic, ripencc). `None` → all TALs.
- **Public types**: `RoaEntry`, `RoaFile` — re-exported at crate root.

### `src/roas_trie.rs` — Core Data Structure

- **`RoasTrie`** — Wraps `IpnetTrie<HashMap<(u8, u32), RoasTrieEntry>>`. Each trie node
  stores a map keyed by `(max_len, origin_asn)` → `RoasTrieEntry`.
- **`RoasTrieEntry`** — Stores `max_len`, `origin`, and date ranges. Uses dual storage:
  `dates: HashSet<i64>` during bootstrap, `dates_compressed: VecDeque<(i64, i64)>` after
  `compress_dates()` merges consecutive days into ranges.
- **Key methods**:
  - `load(path)` / `dump(path)` — Serialize/deserialize via `bincode` + `oneio` (supports
    `.gz` compression).
  - `process_entries(entries, bootstrap)` — Insert ROA entries; `bootstrap=true` uses
    `HashSet` dates, `bootstrap=false` uses compressed `VecDeque`.
  - `search(prefix, origin, max_len, date, current, exact)` — Query with filters.
    `exact=true` (default) uses `exact_match()` for the prefix; `exact=false` uses
    `matches()` which includes supernets and subnets.
  - `update(tal, until)` — Incremental update: crawl new files since `latest_date`,
    process, and update in place.
  - `compress_dates()` — Convert all `HashSet` dates to compressed `VecDeque` ranges.
  - `fill_gaps()` — Fill known historical data gaps (see `KNOWN_GAPS_STR` constant).
  - `validate(prefix, origin, date_ts)` — RPKI validation: `Valid` / `Invalid` / `Unknown`.
  - `lookup_prefix(prefix)` — Return all ROAs matching a prefix (includes super/subnets).

### `src/api.rs` — HTTP API (Axum)

- **`/search`** — Query ROAs. Parameters: `asn`, `prefix`, `max_len`, `date`, `current`,
  `page`, `page_size` (max 1000), `exact` (default `true`). Returns paginated JSON.
  Malformed `prefix` or `date` returns `400` with a JSON error.
- **`/health`** — Returns IPv4/IPv6 ROA counts and `latest_date`.
- **`start_api_service(trie, host, port, root)`** — Starts the Axum server with CORS.
  `root` parameter nests routes under a path prefix (e.g., `/api`).

### `src/bin/main.rs` — CLI

- **Global args**: `path` (default `roas_trie.bin.gz`), `--bootstrap` (download from
  `spaces.bgpkit.org` if file missing), `--env` (dotenv path).
- **Subcommands**:
  - `rebuild` — Full historical rebuild from scratch (parallel, with progress bar).
  - `update` — Incremental update from latest data.
  - `search` — CLI search with `--prefix`, `--asn`, `--max_len`, `--date`, `--current`,
    `--exact`. Outputs a markdown table.
  - `fix` — Fill known data gaps.
  - `serve` — Start the API server. `--host` (default `0.0.0.0`), `--port` (default
    `40065`), `--backup-to` (additional backup path). Background thread updates the trie
    every 8 hours and uploads backups to R2/S3/local.

## Architecture Notes

- **Data flow**: RIPE CSV dumps → `parse_roas_csv` → `RoasTrie.process_entries` →
  in-memory trie → serialized to `.bin.gz` via `bincode`.
- **Bootstrap file**: `https://spaces.bgpkit.org/broker/roas_trie.bin.gz` — pre-built
  trie with full history. Downloaded automatically with `--bootstrap` if no local file.
- **Backup**: `WAYBACK_BACKUP_TO=r2://spaces/broker/roas_trie.bin.gz` uploads to Cloudflare
  R2 after each background update cycle. Requires `AWS_*` env vars.
- **Production deployment**: Docker Compose on `bh-01`/`bh-02` (Tailscale), proxied by
  Cloudflare Worker at `alpha.api.bgpkit.com` with failover.

## Build & Test

```bash
cargo build                              # Build
cargo clippy --all-features -- -D warnings  # Lint (CI-enforced)
cargo test --lib roas_trie::tests         # Run offline unit tests only
cargo test                                # Run all tests (some require network)
```

## CI/CD

- **`rust.yml`** — On PR to main: `cargo build` + `cargo clippy -- -D warnings`.
- **`release.yml`** — On `v*` tag: GitHub release (from `CHANGELOG.md`), `cargo publish`,
  binary uploads for `aarch64-linux`, `x86_64-linux`, `universal-apple-darwin`.
- **`docker.yml`** — On `v*` tag or push to main: builds `linux/amd64` + `linux/arm64`,
  pushes to `bgpkit/wayback-rpki` on Docker Hub with semver tags + `latest`.

## Conventions

- Clippy must pass with `-D warnings` — CI enforces this.
- `sort_by_key` is preferred over `sort_by(|a, b| ...)` (newer clippy lint).
- Type aliases are used for complex generics (see `RoasTrieMap`) to satisfy
  `clippy::type_complexity`.
- The `IpnetTrie::matches()` method returns all children of the shortest supernet, NOT
  just the queried prefix — this caused issue #9. Always use `exact_match()` for exact
  prefix lookups unless inclusive matching is explicitly desired.

## Known Gaps

`KNOWN_GAPS_STR` in `roas_trie.rs` lists dates where RIPE data was missing. The `fix`
subcommand fills these by interpolating adjacent date ranges.
