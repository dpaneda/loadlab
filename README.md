<h1 align="center">loadlab</h1>
<h3 align="center">A programmable Linux workload laboratory</h3>

<p align="center">
<img alt="License" src="https://img.shields.io/badge/license-MIT-blue.svg">
<img alt="Language" src="https://img.shields.io/badge/language-Rust-orange.svg">
<img alt="Platform" src="https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black">
<img alt="MSRV" src="https://img.shields.io/badge/MSRV-1.85-blue.svg">
</p>

## 🌟 Motivation and Description

I wanted to understand how a Linux kernel behaves under a *specific* memory or I/O
workload, and every tool I reached for measured the machine rather than the workload.
`stress-ng` tells me it allocated 4 GiB; it does not tell me that `sum(Rss)` across four
processes says 16 GiB while `sum(Pss)` says 4 GiB.

loadlab is built around one idea:

> Express a desired resource workload as state, change that state over HTTP while the process runs,
> and converge toward it continuously while reporting what the kernel actually measured.

The interesting part is the third clause. Every controller here is a *model* of a mechanism
(arena, mapping, page walk, block read) sitting next to an *observation* of the result
(`/proc/<pid>/smaps_rollup`, fault counters, `schedstat`). A target is never echoed back as an
achievement: `requested` and `actual` are separate fields everywhere, and an unconverged
controller says so rather than rounding up to the target.

## 🤔 Why use loadlab?

Consider using loadlab if you:

 - Want each resource dimension targetable **independently**, rather than by one tool that
   does memory and network together.
 - Need the gap between what you asked for and what the kernel delivered — per controller, live.
 - Want to reproduce page-cache, writeback, reclaim or COW behaviour on purpose, with
   attribution between the thread generating pressure and everything else.
 - Need a run that is **reproducible**: one seed plus one start time identifies it exactly.
 - Prefer a control plane you can `curl` over a config file and a restart.

If you want a throughput number for a database or an HTTP service rather than for the
kernel underneath it, use the tool built for that — `wrk`, `sysbench`, `fio` or `stress-ng`.
loadlab has no client library, no scripting protocol and no reporting; the network generator
speaks raw TCP and UDP and deliberately leaves the application out.

It is a research tool for one host, not a distributed load generator. It binds loopback by
default and allocates real memory on the machine that runs it.

## 👨‍💻 Getting started

1. Build and run. The API comes up on `http://127.0.0.1:8080`:

```bash
cargo run --release
```

2. Ask for some anonymous memory and start it:

```bash
curl -s -X PUT localhost:8080/workload \
  -H 'content-type: application/json' \
  -d '{"anonymous":{"rss":"512MiB"}}'

curl -s -X POST localhost:8080/workload/start
```

3. Read back what the kernel says, not what was requested. The target and the measurement
   are separate fields:

```bash
curl -s localhost:8080/metrics | jq '.controller | {last_target_arena_bytes, arena_resident_bytes, convergence_error_bytes, converged}'
# {
#   "last_target_arena_bytes": 536317952,
#   "arena_resident_bytes": 536317952,
#   "convergence_error_bytes": 0,
#   "converged": true
# }
```

4. Change your mind while it runs. No restart:

```bash
curl -s -X PATCH localhost:8080/workload \
  -H 'content-type: application/json' \
  -d '{"cpu":{"utilization":"250","workers":4}}'
```

## 📊 Usage

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

### What you can drive

| Controller | Knobs |
|---|---|
| anonymous memory | `rss`, `thp` (`madvise`/`always`/`never`) |
| memory access | `pattern` (sequential, random, stride, working_set, hot_cold, cold_scan), `pages_per_second` |
| file-backed memory | `rss`, `pages_per_second`, `access_pattern`, `madvise` |
| shared / copy-on-write | `shared_bytes`, `private_bytes`, `actors`, `cow_write_ratio`, memfd or SysV |
| disk I/O | `read_bps`, `write_bps`, `iops`, `fsync_rate`, `block_size`, `queue_depth`, `pattern`, `direct` |
| network | `tcp`/`udp`, `target`, `concurrency`, `connections_per_second`, `messages_per_second`, `message_size` |
| CPU | `utilization`, `workers`, `cpus`, `affinity` |

### Expressions

Every magnitude that says *how much* or *how fast* takes either a plain number or an
expression over `t`, the age of the run in seconds, so a workload can be a curve rather
than a step. This covers RSS and page rates, `read_bps`/`write_bps`, `iops`, `fsync_rate`,
`cpu.utilization`, the network rates and `cow_write_ratio`:

```json
{ "anonymous": { "rss": "min(2GiB, t * 64MiB)" } }
{ "cpu":        { "utilization": "min(400, t * 80)" } }
{ "network":    { "messages_per_second": "500 + 250*sin(t/286.5)" } }
```

`+ - * /`, `min`, `max`, `sin`, `cos`, `exp`, `log` and constants are available. Byte-valued
fields expand unit suffixes (`"64MiB"`); dimensionless ones reject them. Counts and choices
(`workers`, `concurrency`, `io.block_size`, the seeds) stay literal. See
[the expression contract](docs/src/expressions.md#continuous-magnitudes-a-number-or-an-expression).

### Scenarios

A scenario is a stored, versioned workload you can replay:

```bash
curl -s -X POST localhost:8080/scenarios/load -H 'content-type: application/json' -d '{
  "name": "ramp", "version": "1",
  "workload": {
    "memory": { "anonymous_rss": "min(2GiB, t * 64MiB)" },
    "cpu":    { "utilization": "min(400, t * 80)", "workers": 8 },
    "io":     { "read_bps": 134217728, "write_bps": 67108864, "direct": true }
  }}'

curl -s -X POST localhost:8080/scenarios/run -H 'content-type: application/json' \
  -d '{"name":"ramp"}'
```

A scenario is validated when it is defined, so running one cannot fail for a reason loading it
did not report. Changing a scenario means bumping its version.

### Direct I/O

`io.direct: true` opens the region with `O_DIRECT`, so throughput and latency describe the
block device rather than the page cache. `block_size` then has to be a multiple of 4096, or the
request is refused.

One caveat worth knowing before you draw a conclusion from it: reading a region that has never
been written is nearly free, because the filesystem zero-fills the sparse extent without going
to the device. On this host that reads at `0.46 µs` against `12.90 µs` for a region written
first. Write to the region before reading it if you are measuring read latency.

### A built-in blackhole

There is no need for a server to point at. `--sink` makes loadlab itself a discarding TCP and
UDP endpoint:

```bash
cargo run --release -- --sink 127.0.0.1:9099
```

## ⚙️ Configuration

Every flag has an environment variable of the same name in `LOADLAB_` upper case.

| Flag | Default | Meaning |
|---|---|---|
| `--bind` | `127.0.0.1:8080` | HTTP bind address; keep it on loopback |
| `--max-memory-bytes` | 8 GiB | ceiling on anonymous memory; a request can lower it, never raise it |
| `--max-file-backed-bytes` | 1 GiB | ceiling on the file the file-backed controller creates |
| `--file-backed-dir` | cwd | where those files are created; must be a real filesystem |
| `--max-shared-bytes` | 2 GiB | ceiling on shared **plus** copy-on-write, combined |
| `--max-io-bytes` | 1 GiB | ceiling on the I/O region |
| `--max-network-ops-per-second` | 100000 | ceiling on messages plus fresh connections |
| `--api-token` | unset | Bearer token required on every endpoint but `/health` |
| `--max-body-bytes` | 65536 | largest accepted request body |
| `--mutation-rate-per-second` / `--mutation-burst` | 20 / 40 | mutations accepted before `429` |
| `--seed` / `--start-time` | unset | make a run reproducible; `t = 0` at a fixed RFC 3339 instant |
| `--sink` | unset | run as a discarding TCP/UDP endpoint instead |

### Safety

The ceilings are the point of the tool. Memory and disk are clamped and reported as
`saturated` rather than refused, so an experiment degrades visibly instead of failing
mysteriously. The network ceiling is separate for a different reason: it is the one limit that
protects systems other than the one running loadlab.

The file-backed controller **refuses to start on tmpfs**. Those pages would be accounted as
shared memory rather than file memory, and the workload would silently not be doing what you
asked — the one failure mode worse than an error.

## 🔒 Exposing it beyond loopback

Setting `--bind` to a routable address hands anyone who can reach the port the ability to
allocate memory and generate traffic. If you do it, set a token:

```bash
LOADLAB_API_TOKEN=s3cret cargo run --release -- --bind 0.0.0.0:8080
```

loadlab does not terminate TLS; put it behind a reverse proxy if the port leaves the host.

## 🛠️ Development

```bash
make check     # cargo check
make test      # 229 unit + 102 integration tests
make fmt       # cargo fmt
make smoke     # scripts/smoke.sh against a running instance
make docs      # build the mdBook into docs/book
make run       # cargo run --release
```

`make lint` runs clippy with `-D warnings` and **does not currently pass** — the tree carries
~194 pedantic warnings. Run `cargo clippy --all-targets` to see them.

`examples/basic.sh` is a short end-to-end tour of the API. `scripts/smoke.sh` is what CI-style
checks run.

## 📚 Documentation

The documentation is an [mdBook](https://rust-lang.github.io/mdBook/) under [`docs/`](docs/):
[`docs/src/`](docs/src/) is the source, `make docs` builds it into the (gitignored) `docs/book/`,
and `.github/workflows/docs.yml` publishes it as a static site (GitHub Pages).

| Chapter | Contents |
|---|---|
| [Introduction](docs/src/introduction.md) | what loadlab is and the model it is built around |
| [Quick start](docs/src/getting-started.md) | build it, drive it with `curl`, read it back |
| [Configuration](docs/src/configuration.md) | process flags and their safety ceilings |
| [Continuous magnitudes](docs/src/expressions.md) | the number-or-expression contract |
| [Controllers](docs/src/controllers/cpu.md) | CPU, memory, disk I/O and network, one chapter each |
| [HTTP API](docs/src/http-api/authentication.md) | every endpoint, field and error code |
| [Architecture](docs/src/architecture.md) · [Deploying](docs/src/deploy.md) | internals and systemd |
| [`contrib/`](contrib/) | Grafana dashboard, alert rules, Prometheus and Compose examples |