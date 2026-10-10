#!/usr/bin/env bash
set -euo pipefail

BASE="${LOADLAB_BASE_URL:-http://127.0.0.1:8080}"

curl -fsS "$BASE/health" | jq -e '.status == "ok"' >/dev/null

curl -fsS -X PUT "$BASE/workload" \
  -H 'content-type: application/json' \
  -d '{"running":true,"memory":{"anonymous_rss":"64MiB"}}' >/dev/null

sleep 1
curl -fsS "$BASE/metrics" | jq -e '.process.memory.rss_anon_bytes > 0' >/dev/null

curl -fsS -X PATCH "$BASE/workload" \
  -H 'content-type: application/json' \
  -d '{"memory":{"anonymous_rss":"16MiB"}}' >/dev/null

curl -fsS -X POST "$BASE/workload/stop" | jq -e '.running == false' >/dev/null

echo "loadlab smoke test passed"
