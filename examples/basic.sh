#!/usr/bin/env bash
set -euo pipefail

base='http://127.0.0.1:8080'

curl -sS -X PUT "$base/workload" \
  -H 'content-type: application/json' \
  -d '{
    "running": true,
    "memory": {
      "anonymous_rss": "256MiB"
    }
  }' | jq

sleep 2

curl -sS "$base/metrics" | jq

curl -sS -X PATCH "$base/workload" \
  -H 'content-type: application/json' \
  -d '{
    "memory": {
      "anonymous_rss": "512MiB + 128MiB*sin(t/5)"
    }
  }' | jq

sleep 5
curl -sS "$base/metrics" | jq

curl -sS -X POST "$base/workload/stop" | jq
