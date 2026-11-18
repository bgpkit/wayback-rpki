#!/bin/sh
# Publish the ROA history as a bgpkit-snap object, and optionally the pre-2.0 JSONL transport.
#
# Required environment:
#   WAYBACK_SPANS          Parquet file written by `wayback-pg export-spans`
#   WAYBACK_PUBLISH_URI    s3://bucket/prefix to publish the object and manifest under
# Optional:
#   WAYBACK_LEGACY_JSONL   s3://bucket/key for the JSONL transport older binaries read
#   WAYBACK_SNAP_HEARTBEAT_URL  heartbeat ping URL, fired only when the publish exits 0
#   WAYBACK_BINARY         path to wayback-pg (default: wayback-pg on PATH)
#
# Usage: wayback-snap-publish.sh [--force] [--dry-run]
set -eu

: "${WAYBACK_SPANS:?WAYBACK_SPANS is not set}"
: "${WAYBACK_PUBLISH_URI:?WAYBACK_PUBLISH_URI is not set}"
: "${WAYBACK_BINARY:=wayback-pg}"

set -- --spans "$WAYBACK_SPANS" --publish "$WAYBACK_PUBLISH_URI" "$@"
if [ -n "${WAYBACK_LEGACY_JSONL:-}" ]; then
    set -- "$@" --legacy-jsonl "$WAYBACK_LEGACY_JSONL"
fi

status=0
"$WAYBACK_BINARY" snap-build "$@" || status=$?

# A skipped run (source unchanged) still proves the pipeline is alive; a failed build or upload
# leaves the heartbeat missing, which is what the monitor is for.
if [ "$status" -eq 0 ] && [ -n "${WAYBACK_SNAP_HEARTBEAT_URL:-}" ]; then
    if ! curl -fsS --max-time 10 "$WAYBACK_SNAP_HEARTBEAT_URL" >/dev/null; then
        # the publish succeeded; an unreachable monitor is worth saying out loud but is not a
        # reason to report the publish as failed
        echo "warning: heartbeat ping failed" >&2
    fi
fi

exit "$status"
