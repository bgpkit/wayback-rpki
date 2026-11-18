# Deployment: the daily snap publish

Three files, installed once, then a timer runs the publish every day:

```
wayback-snap-publish.sh   builds the object from the spans export and publishes it
wayback-snap.service      runs the script once, as one unit of work
wayback-snap.timer        fires the service once a day
```

Nothing here names a host, a bucket, or a credential. Every site-specific value comes from
the environment file the service loads, so this directory describes the pipeline and the
operator supplies the addresses.

## What the script needs

| variable | meaning |
|---|---|
| `WAYBACK_SPANS` | the Parquet file `wayback-pg export-spans` writes |
| `WAYBACK_PUBLISH_URI` | `s3://bucket/prefix` the object and manifest are published under |
| `WAYBACK_LEGACY_JSONL` | optional `s3://bucket/key` for the pre-2.0 JSONL transport |
| `WAYBACK_SNAP_HEARTBEAT_URL` | optional heartbeat ping URL, fired only when the publish exits 0 |

`WAYBACK_LEGACY_JSONL` exists because binaries from before 2.0 cannot read a snap object and
bootstrap from the JSONL instead. Leaving it unset publishes only the object.

The heartbeat is optional and has no default. When it is set, a successful publish pings it and
a failed one does not, so a missing heartbeat means the pipeline is not running.

## Install

```sh
install -m 0755 wayback-snap-publish.sh /usr/local/sbin/
install -m 0644 wayback-snap.service wayback-snap.timer /etc/systemd/system/
# the environment file referenced by the service holds the four variables above
systemctl daemon-reload
systemctl enable --now wayback-snap.timer
```

Order the timer after whatever produces the spans export: the script refuses to build from a
file that has not been rewritten, and the publish is skipped (not failed) when the source hash
matches the last published object.

## Verify

```sh
wayback-pg snap-check --pg-config "$PG_CONFIG" \
  --object s3://bucket/prefix/roas-<content id>.snap --sample 200
```

`snap-check` compares a published object with PostgreSQL prefix by prefix and exits non-zero on
a difference. Run it after the first publish and whenever the export SQL changes; the timer
does not run it.
