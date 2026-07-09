# Changelog

All notable changes to this project will be documented in this file.

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