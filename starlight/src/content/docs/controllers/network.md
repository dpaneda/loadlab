---
title: "Network"
---

`network.messages_per_second` and `network.connections_per_second` generate raw TCP or UDP traffic
against a remote target `host:port`. This is deliberately not an HTTP client: the useful question
for a Linux workload laboratory is what the *kernel network stack* does under raw load, leaving
application protocols and HTTP parsing out of it.

A separate **sink mode** turns loadlab itself into a high-performance blackhole that accepts and
discards TCP connections and UDP datagrams on a single address, so experiments can run locally
without an external server and integration tests can execute anywhere.

### Configuration fields

| field | default | constraints | meaning |
| --- | --- | --- | --- |
| `network.transport` | `"tcp"` | `"tcp"` or `"udp"` | transport protocol to generate |
| `network.target` | `""` | `host:port` string or empty | destination; empty generates nothing |
| `network.concurrency` | `1` | `1..=256` | worker slots and concurrent connection pool size |
| `network.connections_per_second` | `0.0` | finite non-negative number, or an expression in `t` | fresh TCP connections to open per second (ignored on UDP) |
| `network.messages_per_second` | `0.0` | finite non-negative number, or an expression in `t` | messages to send per second over persistent connections or UDP |
| `network.message_size` | `1024` | `1..=1048576` bytes | payload size in bytes for each message write and fresh connection |

### Rates follow the run clock

Both rates accept either a constant number or an expression in `t`, the run clock, following
the same [continuous-magnitude contract](../../expressions/#what-t-is-the-run-clock) as every
other rate. The expression is dimensionless -- an operation rate is not a byte count, so `1MiB` is
refused rather than silently read as a million operations per second -- and it is evaluated once per
tick, so the target moves rather than being set at `t = 0`. A network piece of a workload can
therefore breathe with the rest of it:

```json
{"network": {"target": "127.0.0.1:9000",
             "messages_per_second": "500 + 250*sin(t/286.5)"}}
```

This is `mean + amplitude*sin(t/N)`, so the period is `2*pi*N` seconds: `N = 286.5` is one cycle every
30 minutes. As everywhere else, a value that dips negative is clamped to zero at runtime, while a
value that is negative at `t = 0` is a `400`. A constant stays a number in `GET /state`; an
expression round-trips as the string it was written as.

### Rates are measured, not modelled

Every number reported as *actual* is the sum of what the sockets' `write` or `send` calls returned
divided by the tick's wall time. A model would keep reporting the target through a congested link
or a peer that stopped reading; measuring exposes the gap between the target and the stack's real
throughput.

A per-tick PI feedback loop (`RATE_KP = 0.1`, capped integral) adjusts the per-tick operation budget
towards the requested rates so the generator converges smoothly. `loadlab_network_converged` reports
whether measured rates are within `RATE_TOLERANCE` (1.0 ops/s) of requested rates.

### Messages vs fresh connections

- **Messages (`messages_per_second`)**: On TCP, messages are sent as `message_size`-byte writes over a
  pool of `concurrency` long-lived, persistent connections. On UDP, each message is an independent
  datagram sent via connected UDP sockets.
- **Connections (`connections_per_second`)**: Fresh, short-lived TCP connections opened, written to once
  with `message_size` bytes, and closed. Connection churn exercises a different part of the network
  stack (SYN handshake, accept queue, socket allocation, teardown) than bulk transfers over
  established connections. Fresh connections contribute their bytes to `bytes_total` and
  `bytes_per_second_actual`, but not to `messages_total`, keeping the two targets independent.

### Bounded operations and safety

Every socket operation is bounded in time:
- Single TCP `connect` timeout: 1 second (`CONNECT_TIMEOUT`).
- Socket `write` or `send` timeout: 1 second (`WRITE_TIMEOUT`).

This prevents an unreachable target or an unreading peer from stalling the controller tick loop.
A safety valve of `MAX_OPS_PER_TICK` (1,000,000 ops/tick) protects against nonsensical configurations.

### Operator ceiling

```bash
cargo run --release -- --max-network-ops-per-second 100000   # or LOADLAB_MAX_NETWORK_OPS_PER_SECOND
```

Defaults to 100,000 ops/second. Because network traffic leaves the host, this safety limit protects
external systems and network infrastructure from being accidentally flooded. Requests exceeding this
limit are clamped to the ceiling and reported as `saturated: true` (`loadlab_network_saturated == 1`),
similar to an over-capacity CPU target.

### Sink mode (blackhole)

```bash
cargo run --release -- --sink 127.0.0.1:9000   # or LOADLAB_SINK
```

When `--sink <SocketAddr>` is supplied, loadlab runs as a discarding sink instead of starting the HTTP
API and workload controller. It binds both a TCP listener and a UDP socket on the specified address:
- TCP: Accepts incoming connections concurrently, continuously reading and discarding all received
  bytes until EOF or error.
- UDP: Continuously receives and discards datagrams using a 64 KiB buffer.

This allows network generators to point to `127.0.0.1:9000` for self-contained, reproducible load tests.

### What the controller reports

Under `controller.network` in `GET /metrics`:

| field | meaning |
| --- | --- |
| `transport` | transport protocol (`"tcp"` or `"udp"`) |
| `target` | target destination as `host:port`, or `""` when none is configured |
| `concurrency` | active worker slots / concurrent sockets |
| `message_size_bytes` | byte size of each message payload |
| `connections_per_second` / `connections_per_second_actual` | requested and measured fresh TCP connection rates |
| `messages_per_second` / `messages_per_second_actual` | requested and measured message throughput |
| `bytes_per_second_actual` | measured total byte throughput (includes messages and fresh connections) |
| `messages_total` | cumulative messages sent over persistent connections or UDP |
| `bytes_total` | cumulative bytes accepted by sockets |
| `connections_total` | cumulative fresh TCP connections established |
| `errors_total` | cumulative failed socket operations (connect, write, send) |
| `converged` | whether all requested rates are within tolerance of measured rates |
| `saturated` | whether the request was clamped by the operator's ops/second ceiling |

