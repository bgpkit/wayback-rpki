# Changelog

All notable changes to this project will be documented in this file.

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