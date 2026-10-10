# Quick start

## Build and run

The API comes up on `http://127.0.0.1:8080`:

```bash
cargo run --release
```

## Ask for some anonymous memory and start it

```bash
curl -s -X PUT localhost:8080/workload \
  -H 'content-type: application/json' \
  -d '{"anonymous":{"rss":"512MiB"}}'

curl -s -X POST localhost:8080/workload/start
```

## Read back what the kernel says, not what was requested

The target and the measurement are separate fields:

```bash
curl -s localhost:8080/metrics | jq '.controller | {last_target_arena_bytes, arena_resident_bytes, convergence_error_bytes, converged}'
# {
#   "last_target_arena_bytes": 536317952,
#   "arena_resident_bytes": 536317952,
#   "convergence_error_bytes": 0,
#   "converged": true
# }
```

## Change your mind while it runs

No restart needed:

```bash
curl -s -X PATCH localhost:8080/workload \
  -H 'content-type: application/json' \
  -d '{"cpu":{"utilization":"250","workers":4}}'
```

## Endpoints

| Endpoint | What it does |
|---|---|
| `PUT /workload` | replace the whole desired state |
| `PATCH /workload` | merge a partial update into it |
| `POST /workload/start` / `stop` | run or halt the workload |
| `GET /state` | the desired state, normalized |
| `GET /metrics` | JSON: host identity, the run's identity, and every observation |
| `GET /prometheus` | the same numbers as a Prometheus exposition |
| `POST /scenarios/load` / `run` / `stop`, `GET /scenarios` | named, versioned workloads |
| `GET /health` | liveness; the one endpoint that never needs a token |

## What you can drive

| Controller | Knobs |
|---|---|
| anonymous memory | `rss`, `thp` (`madvise`/`always`/`never`) |
| memory access | `pattern` (sequential, random, stride, working_set, hot_cold, cold_scan), `pages_per_second` |
| file-backed memory | `rss`, `pages_per_second`, `access_pattern`, `madvise` |
| shared / copy-on-write | `shared_bytes`, `private_bytes`, `actors`, `cow_write_ratio`, memfd or SysV |
| disk I/O | `read_bps`, `write_bps`, `iops`, `fsync_rate`, `block_size`, `queue_depth`, `pattern`, `direct` |
| network | `tcp`/`udp`, `target`, `concurrency`, `connections_per_second`, `messages_per_second`, `message_size` |
| CPU | `utilization`, `workers`, `cpus`, `affinity` |

## A built-in blackhole

There is no need for a server to point at. `--sink` makes loadlab itself a discarding TCP and UDP
endpoint:

```bash
cargo run --release -- --sink 127.0.0.1:9099
```
