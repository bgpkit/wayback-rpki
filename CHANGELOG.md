# Changelog

All notable changes to this project will be documented in this file.

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