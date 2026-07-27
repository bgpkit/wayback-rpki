# Development Notes

This file records reproducible development and data-repair operations for
`wayback-rpki`. It is not a production runbook: do not upload, replace remote
artifacts, or change a deployed service unless that operation is explicitly
approved.

## Historical ROA gap investigation

A discontinuity in a stored ROA date range is a candidate, not proof that the
archive has a gap. A real withdrawal followed by a later reauthorization has
the same shape.

For a candidate date `D`, compare the published transport with the exact five
crawler inputs on `D-1`, `D`, and `D+1`:

```text
https://ftp.ripe.net/rpki/<tal>.tal/YYYY/MM/DD/roas.csv.xz
```

Use the five TALs `afrinic`, `apnic`, `arin`, `lacnic`, and `ripencc`. Report
both distinct `(prefix, maxLength, ASN)` record counts and distinct prefix
counts, separately per TAL and for their union.

Classify each date before repairing it:

- **Product-history omission:** the published transport differs from available
  raw input.
- **Raw-source discontinuity:** the transport matches a reduced or empty raw
  TAL snapshot.
- **Both:** different TALs exhibit both conditions on the same date.

An HTTP 200 response alone is not evidence that an archived snapshot is
complete or nonempty. Treat a gap bridged by `fill_gaps()` as **synthetic
interpolation** when the raw source was reduced or absent; do not describe it
as an observed ROA state.

## Updating the known-gap policy

`src/roas_trie.rs` contains `KNOWN_GAPS_STR`. `RoasTrieMut::fill_gaps()` fills
a listed interval only for an identical `(prefix, maxLength, ASN)` record that
exists immediately before and after the interval. It therefore preserves
continuity only where both boundary observations exist.

Before adding a date to the constant, record:

1. date interval and affected TAL(s);
2. product-history/raw-source classification;
3. representative corrected and unaffected lookup checks; and
4. whether the result is recovered source data or synthetic interpolation.

Add a regression fixture for any new policy date.

## Generating a fixed portable transport

The `fix` subcommand imports a JSONL transport, applies the currently compiled
`KNOWN_GAPS_STR`, and exports a new portable JSONL transport. It does not modify
its input when `--output` is specified. Its temporary rkyv archive is local and
is deleted after export.

After changing `KNOWN_GAPS_STR`, run:

```bash
wayback-rpki fix /tmp/roas_trie.jsonl.gz -o /tmp/roas_trie.fixed.jsonl.gz

gzip -t /tmp/roas_trie.fixed.jsonl.gz
sha256sum /tmp/roas_trie.jsonl.gz /tmp/roas_trie.fixed.jsonl.gz
```

Without `-o/--output`, the input is overwritten in place via an atomic rename.

Before any separately approved publication, preserve the input and output
hashes, code revision, exact gap list, and query evidence. Keep the portable
`.jsonl.gz` as the transport artifact: `.rkyv` files are local, architecture-
specific mmap archives.

## Validation

Run the focused subcommand test and repository quality gates after editing the
gap policy or transport-generation implementation:

```bash
cargo test --bin wayback-rpki fix_bridges_a_known_gap
cargo fmt --check
cargo clippy --all-features -- -D warnings
cargo test --all-features
```

For a candidate artifact, validate both corrected historical dates and control
dates that must remain unchanged. Check the decompressed transport with
`gzip -t`; retain its SHA-256 with the investigation record.
