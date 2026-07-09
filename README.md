# Wayback RPKI Database

A command-line tool and API service for querying historical RPKI ROA (Route Origin
Authorization) data. It crawls daily ROA dumps from RIPE
(`https://ftp.ripe.net/rpki/`), stores them in a memory-efficient prefix trie, and
serves a REST API for looking up ROA history by prefix, ASN, max-length, date, and
currency status.

## Features

- **Historical ROA data** — Full history from 2011 onward, sourced from all 5 RIR TALs
  (Afrinic, APNIC, ARIN, LACNIC, RIPE NCC)
- **Fast prefix trie** — `ipnet-trie` backed storage with compressed date ranges for
  efficient memory usage (~1M+ ROAs in memory)
- **REST API** — Query by prefix, ASN, max-length, date, and current/expired status
- **Incremental updates** — Background thread fetches new data every 8 hours
- **Backup to R2/S3** — Automatic backup of the trie to Cloudflare R2 or any S3-compatible
  storage
- **One-command bootstrap** — Download a pre-built trie and start serving immediately
- **Docker deployment** — Multi-arch image (`linux/amd64`, `linux/arm64`)

## Install

### Using `cargo`

```bash
cargo install wayback-rpki
```

### Using `homebrew` on macOS

```bash
brew install bgpkit/tap/wayback-rpki
```

### Using [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall)

```bash
cargo binstall wayback-rpki
```

### Using Docker

```bash
docker run -p 40065:40065 bgpkit/wayback-rpki:latest
```

## Quick Start

Start the API with a one-command bootstrap (downloads pre-built trie from
`spaces.bgpkit.org`):

```bash
wayback-rpki serve --bootstrap
```

This downloads the bootstrap trie file, loads it into memory, and starts an HTTP API
server on `0.0.0.0:40065`. The server will automatically update its data every 8 hours.

## CLI Usage

```
wayback-rpki [OPTIONS] <COMMAND>

Commands:
  rebuild   Rebuild the entire RPKI ROA history data from scratch
  update    Find new ROA files and apply incremental changes
  search    Search for ROAs in history
  fix       Fill known historical data gaps
  serve     Start the API server

Options:
  -p, --path <PATH>      File path to the trie data file [default: roas_trie.bin.gz]
  -b, --bootstrap        Download bootstrap file if the data file doesn't exist
      --env <ENV>        Path to an environment variable file
  -h, --help             Print help
  -V, --version          Print version
```

### `rebuild` — Full Historical Rebuild

Crawls all ROA dumps from RIPE since 2011 and builds the trie from scratch.

```bash
wayback-rpki rebuild --from 2020-01-01 --until 2024-12-31
```

Options: `--tal` (filter to one RIR), `--from`, `--until`, `--chunks` (parallelism,
defaults to CPU count).

### `update` — Incremental Update

Fetches only new ROA files since the trie's latest date:

```bash
wayback-rpki update
```

### `search` — CLI Search

Query ROAs from the command line. Outputs a markdown table.

```bash
# Search for exact prefix
wayback-rpki search --prefix 1.1.1.0/24

# Search by ASN, only current ROAs
wayback-rpki search --asn 13335 --current true

# Search with date filter
wayback-rpki search --prefix 1.1.1.0/24 --date 2020-06-01

# Include supernets and subnets (non-exact matching)
wayback-rpki search --prefix 193.0.14.0/24 --exact false
```

Options: `--asn`, `--prefix`, `--max-len`, `--date` (YYYY-MM-DD), `--current` (bool),
`--exact` (bool, default `true`).

### `serve` — API Server

```bash
wayback-rpki serve --bootstrap --host 0.0.0.0 --port 40065
```

Options: `--host` (default `0.0.0.0`), `--port` (default `40065`), `--backup-to` (additional
backup destination path or S3 URL).

## API Reference

### `GET /search`

Query ROAs with optional filters. All parameters are optional.

| Parameter    | Type    | Default | Description |
|-------------|---------|---------|-------------|
| `prefix`    | string  | —       | IP prefix to search, e.g. `1.1.1.0/24` |
| `asn`       | integer | —       | Filter by origin ASN (exact match) |
| `max_len`   | integer | —       | Filter by ROA max-length value |
| `date`      | string  | —       | Date filter (YYYY-MM-DD); returns ROAs active on that date |
| `current`   | boolean | —       | `true`: only current ROAs; `false`: only expired ROAs |
| `exact`     | boolean | `true`  | `true`: exact prefix match only; `false`: include supernets and subnets |
| `page`      | integer | `0`     | Page number (0-indexed) |
| `page_size` | integer | `100`   | Items per page (max 1000) |

**Response:**

```json
{
  "count": 1,
  "error": null,
  "data": [
    {
      "prefix": "1.1.1.0/24",
      "max_len": 24,
      "asn": 13335,
      "date_ranges": [
        ["2018-04-05", "2026-04-03"],
        ["2026-04-05", "2026-06-12"],
        ["2026-06-14", "2026-07-09"]
      ],
      "current": true
    }
  ],
  "meta": { "latest_date": "2026-07-09" },
  "page": 0,
  "page_size": 100
}
```

**Examples:**

```bash
# Exact prefix search (default)
curl "http://localhost:40065/search?prefix=1.1.1.0/24"

# Search by ASN, current ROAs only, first 10
curl "http://localhost:40065/search?asn=13335&current=true&page_size=10"

# ROAs active on a specific date
curl "http://localhost:40065/search?prefix=1.1.1.0/24&date=2020-06-01"

# Include supernets and subnets
curl "http://localhost:40065/search?prefix=193.0.14.0/24&exact=false"
```

**Error handling:** Malformed `prefix` or `date` values return `400 Bad Request` with a
JSON error body (e.g., `{"error": "invalid prefix"}`).

### `GET /health`

Returns trie statistics.

```json
{
  "ipv4_roas_count": 820313,
  "ipv6_roas_count": 279184,
  "latest_date": "2026-07-09"
}
```

## Library API

The crate can be used as a Rust library. The key types and functions are re-exported at
the crate root:

### Crawler Functions (`lib.rs`)

```rust
use wayback_rpki::{crawl_tal_after, parse_roas_csv, get_tal_urls, RoaEntry, RoaFile};
```

- **`crawl_tal_after(tal_url, from, until) -> Vec<RoaFile>`** — Discover ROA dump files
  for a TAL within a date range.
- **`parse_roas_csv(url) -> Result<Vec<RoaEntry>>`** — Download and parse a single
  `roas.csv.xz` file.
- **`get_tal_urls(tal: Option<String>) -> Vec<String>`** — Get RIPE RPKI TAL URLs for
  one or all RIRs.

### Trie Storage (`roas_trie.rs`)

```rust
use wayback_rpki::RoasTrie;
use ipnet::IpNet;
use chrono::NaiveDate;

// Load a pre-built trie
let trie = RoasTrie::load("roas_trie.bin.gz")?;

// Search with filters (exact=true by default)
let results = trie.search(
    Some("1.1.1.0/24".parse().unwrap()),  // prefix
    Some(13335),                           // origin ASN
    None,                                  // max_len
    None,                                  // date
    Some(true),                            // current only
    true,                                  // exact prefix match
);

for entry in &results {
    println!("{} AS{} max_len={}", entry.prefix, entry.origin, entry.max_len);
    for (start, end) in &entry.dates_ranges {
        println!("  active: {} to {}", start, end);
    }
}

// RPKI validation
use wayback_rpki::RpkiValidation;
let date_ts = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap()
    .and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
let validation = trie.validate(&"1.1.1.0/24".parse().unwrap(), 13335, date_ts);
// → RpkiValidation::Valid

// Save the trie
trie.dump("roas_trie.bin.gz")?;
```

#### Key `RoasTrie` Methods

| Method | Description |
|--------|-------------|
| `new()` | Create an empty trie |
| `load(path)` | Load from a `.bin.gz` file |
| `dump(path)` | Save to a `.bin.gz` file |
| `process_entries(entries, bootstrap)` | Insert ROA entries |
| `search(prefix, origin, max_len, date, current, exact)` | Query with filters |
| `lookup_prefix(prefix)` | Get all ROAs for a prefix (includes super/subnets) |
| `validate(prefix, origin, date_ts)` | RPKI validation → `Valid` / `Invalid` / `Unknown` |
| `update(tal, until)` | Incremental update from RIPE |
| `compress_dates()` | Merge consecutive dates into ranges |
| `fill_gaps()` | Fill known historical data gaps |

## Configuration

The `serve` subcommand supports the following environment variables:

| Variable | Description |
|----------|-------------|
| `WAYBACK_BACKUP_TO` | Backup destination (`r2://bucket/key` for S3/R2, or local path) |
| `WAYBACK_BACKUP_HEARTBEAT_URL` | URL to ping after successful backup (e.g., UptimeRobot) |
| `AWS_REGION` | S3/R2 region (required for R2 backups) |
| `AWS_ENDPOINT` | S3/R2 endpoint (required for R2 backups) |
| `AWS_ACCESS_KEY_ID` | S3/R2 access key (required for R2 backups) |
| `AWS_SECRET_ACCESS_KEY` | S3/R2 secret key (required for R2 backups) |

## Docker Deployment

```bash
# Build
docker build -t wayback-rpki .

# Run (bootstraps automatically)
docker run -d -p 40065:40065 wayback-rpki

# Run with R2 backup
docker run -d -p 40065:40065 \
  -e WAYBACK_BACKUP_TO=r2://spaces/broker/roas_trie.bin.gz \
  -e AWS_REGION=auto \
  -e AWS_ENDPOINT=https://<account>.r2.cloudflarestorage.com \
  -e AWS_ACCESS_KEY_ID=<key> \
  -e AWS_SECRET_ACCESS_KEY=<secret> \
  wayback-rpki
```

The Docker image is also published to Docker Hub:

```bash
docker pull bgpkit/wayback-rpki:latest
```

## Data Source

ROA data is crawled from the RIPE RPKI archive:
`https://ftp.ripe.net/rpki/<tal>/YYYY/MM/DD/roas.csv.xz`

Supported TALs: `afrinic`, `apnic`, `arin`, `lacnic`, `ripencc`.

## License

MIT
