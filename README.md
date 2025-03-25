# Wayback RPKI Database

This project implements the crawler of RIPE RIS RPKI daily dump (https://ftp.ripe.net/rpki/) with a database
schema designed to hold historical information.

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

Install `cargo-binstall` first:

```bash
cargo install cargo-binstall
```

Then install `monocle` using `cargo binstall`

```bash
cargo binstall wayback-rpki
```

## Usage

Start the API from scratch by running

```bash
wayback-rpki serve --bootstrap
```

Configure the following environment variables to further configure features:

- `WAYBACK_BACKUP_TO`: backup location
    - if locations starts with r2/s3, such as `r2://spaces/broker/roas_trie.bin.gz`, it will require additional S3
      credentials below
        - `AWS_REGION`
        - `AWS_ENDPOINT`
        - `AWS_ACCESS_KEY_ID`
        - `AWS_SECRET_ACCESS_KEY`
- `WAYBACK_BACKUP_HEARTBEAT_URL`: a URL to send an HTTP get request to