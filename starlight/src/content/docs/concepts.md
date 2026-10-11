---
title: "Core concepts"
---

This page is the whole mental model. Read it once and everything else is details.

## The three ideas

**1. State, not commands.** You do not tell loadlab to "run stress for a minute". You describe a
workload — `memory.anonymous_rss: "512MiB"`, `cpu.utilization: 200` — and loadlab converges to it.
The description is JSON, it can change over HTTP while the process runs, and `PUT`/`PATCH`
semantics make "remove this controller" a one-line mutation.

**2. Every controller is a model next to an observation.** The target is never echoed as an
achievement. `requested` and `actual` are separate fields everywhere: what you asked for, and what
the kernel measured (via `/proc`). An unconverged controller says so instead of rounding up to its
setpoint.

**3. Continuous magnitudes are expressions in `t`.** `t` is seconds since the workload started.
Instead of a cold number, every byte/rate/percentage target can be a small program in `t`, so the
workload itself can be a shape: a ramp, a sinusoid, a plateau.

## Controllers at a glance

| controller | what it is for | what it drives | key knobs |
| --- | --- | --- | --- |
| `cpu` | burn CPU | worker threads spinning | `utilization`, `workers`, `cpus` |
| `memory` | anonymous memory | an `mmap`-ed arena, grown and shrunk | `anonymous_rss`, `thp` |
| `memory_access` | page faults | a walk across the arena's pages | `pattern`, `pages_per_second` |
| `file_backed` | file-backed memory + disk space | a real file mapped `MAP_SHARED` | `rss`, `pages_per_second` |
| `shared` | shared memory and copy-on-write | real processes mapping one segment plus private copies | `shared_bytes`, `private_bytes`, `actors` |
| `io` | disk I/O | reads, writes and `fsync` on a scratch file | `read_bps`, `write_bps`, `iops` |
| `network` | network traffic | TCP/UDP connections and messages to a target | `connections_per_second`, `messages_per_second` |

Every one of them defaults to doing nothing, and every one is a chapter of its own in
[Controllers](../controllers/overview/).

## Expressions at a glance

Any magnitude that says *how much* or *how fast* takes either a plain number or a string expression
in `t`:

```json
"memory.anonymous_rss":   "64MiB + 32MiB*sin(t/10)",
"cpu.utilization":        "min(400, t * 80)",
"network.messages_per_second": "500 + 250*sin(t/286.5)"
```

A `t`-dependent target is evaluated **once per tick** (every `controller.tick_ms`, by default
100 ms), so the workload can chase a curve, not just sit on a number. Details and the full function
list live in [Expressions](../expressions/).

## A few conventions that span everything

- **Nothing happens until `running: true`** — and even then, an unset controller generates nothing.
- **Clamping, not failing.** A request above capacity is clamped and reported `saturated`, not
  rejected.
- **A value that dips negative is not an error.** A sinusoid crosses zero; the controller clamps
  rates and utilizations to zero at runtime. Only a value that is already negative or non-finite at
  `t = 0` is refused with a `400`.
- **Everything round-trips.** `GET /state` echoes exactly what was accepted — an expression comes
  back as the string it was written as — so reading state and writing it back is a no-op.