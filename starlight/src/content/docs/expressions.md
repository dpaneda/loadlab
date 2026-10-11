---
title: "Expressions"
---

Every magnitude that describes *how much* or *how fast* accepts the same two spellings, so there
is one rule to learn rather than one per controller:

- a plain JSON **number** — `8388608`, `80`, `0.5`; or
- a **string expression in `t`** — `"500 + 250*sin(t/286.5)"`, `"64MiB + 16MiB*sin(t/10)"`.

## What `t` is: the run clock

`t` is the run clock: seconds **since the workload started**. It is the same `t` for every
controller in one process, which is what lets independent targets stay in phase — two sinusoids
written with the same `t/60` peak and trough together.

Three consequences that surprise people once:

- `t = 0` at start, and **`t` restarts when the workload restarts**. Stopping and starting again
  rewinds every expression. `t` is therefore *not* process uptime, and it is not wall-clock time.
- `t` is evaluated **once per controller tick**, every `controller.tick_ms` (default 100 ms). The
  workload chases the curve; it does not jump through arbitrary values in between.
- A long run is a long `t`, and an unbounded expression eventually overflows real arithmetic.
  `exp(t*10)` is fine for a minute and then grows past every representable value; prefer bounded
  shapes (`min`, `sin`, division by `t + constant`).

To get a full parabola over a cycle, remember `sin(t / d)` has period `2 * pi * d` seconds, and
`mean + amplitude*sin(t/d)` only stays non-negative when `mean >= amplitude`.

## Units follow the field

A **byte-valued field** expands unit suffixes on its numbers, so `"64MiB + 16MiB*sin(t/10)"` is a
valid `file_backed.rss`. Supported units, case-insensitive:

| decimal | binary |
| --- | --- |
| `B` = 1 | `KiB` = 1024 |
| `KB` = 1000 | `MiB` = 1024^2 |
| `MB` = 1000^2 | `GiB` = 1024^3 |
| `GB` = 1000^3 | `TiB` = 1024^4 |
| `TB` = 1000^4 | |

A **dimensionless field** (`cpu.utilization`, `io.iops`, `io.fsync_rate`, the page rates, the
network rates, `cow_write_ratio`) rejects byte units on purpose: `"1MiB"` on `cpu.utilization` is
a `400`, not a utilization of 1,048,576 percent — the unit table is simply *not applied* there.
Scientific notation still works: `"1e2"` is `100`.

## The grammar

The expression engine is [`meval`](https://docs.rs/meval), which gives this vocabulary:

- **Variable:** `t`.
- **Operators:** `+`, `-`, `*`, `/`, `%` (modulo), `^` (power), unary minus, and parentheses.
- **Functions:** `abs`, `min`, `max`, `sqrt`, `exp`, `log` (natural), `log10`, `sin`, `cos`,
  `tan`, `asin`, `acos`, `atan`, `atan2`, `sinh`, `cosh`, `tanh`, `asinh`, `acosh`, `atanh`,
  `floor`, `ceil`, `round`, `signum`.
- **Constants:** `pi`, `e`, `phi`, `ln2`, `ln10`, `log2e`, `log10e`, `sqrt2`, `sqrt1_2`.

Write the multiplication explicitly: `sin(t/10)` and `50*sin(t/10)` are valid; a bare
`sin` without an argument is not.

## Validation and clamping

**Validation happens at `t = 0`.** When the state is accepted, the expression is parsed, `t` is
bound, and the value is evaluated at `t = 0`:

- a **malformed** expression is a `400` that echoes the offending text;
- a value that is **negative or non-finite at `t = 0`** is refused.

A negative value *later* is a normal thing to write — a sinusoid crosses zero — and is clamped by
the controller at runtime: rates and utilizations clamp to `0`, `cow_write_ratio` clamps to
`1.0`. What must not happen is a controller steering on a negative duty cycle or a negative byte
allocation, and it does not.

A value that goes **non-finite partway through a run** (again: unbounded growth in `t`) makes that
tick's reconciliation fail. The failure is logged and reported — `controller_error` and the
`reconcile_errors` counter — rather than silently fed to the controller.

## The spelling round-trips

`GET /state` echoes what was configured, verbatim: an expression comes back as the exact string it
was written as, an integer stays an integer (`4096`, not `4096.0`), and a fractional number stays
fractional. Reading `/state` and writing it back is a no-op — including for expressions, which is
how a dashboard can round-trip a workload without ever parsing it.

## Which fields take which spelling

| kind | fields |
| --- | --- |
| **magnitude** (number *or* expression) | `memory.anonymous_rss`, `memory_access.pages_per_second`, `file_backed.rss`, `file_backed.pages_per_second`, `shared.shared_bytes`, `shared.private_bytes`, `shared.cow_write_ratio`, `io.read_bps`, `io.write_bps`, `io.iops`, `io.fsync_rate`, `cpu.utilization`, `network.connections_per_second`, `network.messages_per_second` |
| **count or choice** (literal only) | `running`, `workers`, `cpus`, `affinity`, `concurrency`, `actors`, `block_size`, `queue_depth`, `message_size`, `backing`, `transport`, `madvise`, `thp`, `pattern` / `access_pattern`, `working_set_pages`, `stride_pages`, `stride_blocks`, the seeds |

A worker count is not a magnitude that ramps over time, and a `tcp`/`udp` choice is not a number.
An expression on one of those is a `400`, with the same honesty as a byte unit on a percentage.