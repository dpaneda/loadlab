---
title: "Going further: workloads with expressions"
---

The quick start drove one cold number. Everything below is richer, but every example is just the
same two facts: state is JSON, and a target can be an expression in `t`. Start loadlab once
(`cargo run --release`) and drive it with `curl`.

## A ramp that plateaus

A workload that gradually grows to full pressure, then holds. `t` starts at 0 on start and counts
seconds; `min` caps the ramp at its ceiling.

```bash
curl -s -X PUT localhost:8080/workload \
  -H 'content-type: application/json' \
  -d '{"running":true,
       "cpu":{"utilization":"min(400, t * 80)","workers":4}}'
```

For the five seconds it takes `t * 80` to reach 400 against four workers, `requested_utilization_pct`
climbs from `t * 80` itself; after that the request is above pool capacity and the controller walks
from a climbing request to a **saturated** steady state:

```bash
curl -s localhost:8080/metrics | jq '.controller.cpu | {requested_utilization_pct, target_utilization_pct, actual_utilization_pct, saturated, converged}'
```

That is the pattern to copy for anything that should ramp up and hold: `min(<ceiling>, t * <rate>)`.
The same shape works for bytes — `min(1GiB, t * 64MiB)` on `memory.anonymous_rss` grows the arena
to 1 GiB over sixteen seconds and stays there.

## Two resources that breathe together

A sinusoid makes a target rise and fall on its own schedule. The two resources share a phase, which
is what "together" means: the same `t/60` clock drives both, so the arena peaks at the same moment
the CPU pool does.

```bash
curl -s -X PUT localhost:8080/workload \
  -H 'content-type: application/json' \
  -d '{"running":true,
       "memory":{"anonymous_rss":"256MiB + 128MiB*sin(t/60)"},
       "cpu":{"utilization":"150 + 50*sin(t/60)","workers":2}}'
```

`GET /metrics` shows the memory controller converging to a moving arena target and the CPU pool
tracking the moving utilization; a healthy tracking workload is one where `requested` and `actual`
stay close and `converged` stays true as the phase passes through zero and over the peak. The period
is `2 * pi * 60` seconds: roughly six minutes for a full cycle.

## Make the network breathe against a sink

There is no server to set up. Start loadlab with `--sink`, and it becomes the discarding TCP/UDP
endpoint it sends traffic to:

```bash
cargo run --release -- --sink 127.0.0.1:9099
```

Then point the network controller at it with a slowly breathing message rate:

```bash
curl -s -X PUT localhost:8080/workload \
  -H 'content-type: application/json' \
  -d '{"running":true,
       "network":{"transport":"tcp","target":"127.0.0.1:9099",
                  "messages_per_second":"500 + 250*sin(t/286.5)",
                  "message_size":1024}}'
```

The full cycle takes `2 * pi * 286.5` seconds, or half an hour — an hour-scale test of a service
behind the traffic. Watch `controller.network`:

```bash
curl -s localhost:8080/metrics | jq '.controller.network | {messages_per_second, messages_per_second_actual, bytes_per_second_actual, saturated}'
```

`bytes_per_second_actual` answers the bandwidth question the same body already contains: at the
750 msgs/s peak with 1 KiB messages, roughly 0.73 MiB/s.

## Layering a second controller on top

`PATCH /workload` merges, so the network workload from above can keep running while disk I/O joins
it. Nothing restarts, and the network target is not re-negotiated:

```bash
curl -s -X PATCH localhost:8080/workload \
  -H 'content-type: application/json' \
  -d '{"io":{"write_bps":"8MiB","queue_depth":8,"block_size":4096}}'
```

Now `GET /metrics` exposes `controller.io` beside `controller.network` — the stream to the sink, a
write pulls through the page cache to a scratch file, and each keeps its own `actual` vs `target`.

## The flagship: shared memory and copy-on-write

This is the difference loadlab exists to make visible. One `512MiB` segment, four real processes
mapping it shared, and each process writing half of a `128MiB` private copy:

```bash
curl -s -X PUT localhost:8080/workload \
  -H 'content-type: application/json' \
  -d '{"running":true,
       "shared":{"shared_bytes":"512MiB","private_bytes":"128MiB",
                 "actors":4,"cow_write_ratio":0.5}}'
```

Read the accounting:

```bash
curl -s localhost:8080/metrics | jq '.controller.shared | {shared_rss_bytes, shared_overcount_bytes, accounted_bytes, physical_bytes, converged}'
```

Naive accounting (`accounted_bytes`) says four actors each count the shared segment, so the machine
looks 2 GiB used. The actual physical cost (`physical_bytes`) is the segment once, plus the
copy-on-write pages the actors really own. `shared_overcount_bytes` is exactly the difference this
controller exists to expose — see [Shared memory](../controllers/shared/) for why.