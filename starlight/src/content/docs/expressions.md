---
title: "Continuous magnitudes: a number or an expression"
---

Every magnitude that describes *how much* or *how fast* accepts the same two spellings, so there is
one rule to learn rather than one per controller:

- a plain JSON **number** — `8388608`, `80`, `0.5`; or
- a **string expression in `t`** — `"500 + 250*sin(t/286.5)"`, `"64MiB + 16MiB*sin(t/10)"`.

`t` is the experiment clock, in seconds, and the expression is evaluated once per tick. Fields that
take a number-or-expression: `memory.anonymous_rss`, `memory_access.pages_per_second`,
`file_backed.rss`, `file_backed.pages_per_second`, `shared.shared_bytes`, `shared.private_bytes`,
`shared.cow_write_ratio`, `io.read_bps`, `io.write_bps`, `io.iops`, `io.fsync_rate`,
`cpu.utilization`, `network.connections_per_second` and `network.messages_per_second`.

Everything else is a **count or a choice** and stays literal: `workers`, `concurrency`, `actors`,
`block_size`, `queue_depth`, `message_size`, `working_set_pages`, `stride_pages`, `stride_blocks`,
`cpus` and the seeds. A worker count is not a magnitude that ramps over time, and a `UDP`/`TCP`
choice is not a number.

**Units follow the field.** A byte-valued field expands unit suffixes, so `"64MiB + 16MiB*sin(t/10)"`
is a valid `file_backed.rss`, and `read_bps`/`write_bps` accept `"8MiB"`. A dimensionless field
(`cpu.utilization`, `io.iops`, `io.fsync_rate`, the network rates, the page rates, `cow_write_ratio`)
rejects them: `"1MiB"` on `cpu.utilization` is a `400`, not a utilization of one million percent.

**The spelling round-trips.** `GET /state` echoes what was configured: an expression comes back as
the exact string it was written as, an integer stays an integer (`4096`, not `4096.0`), and a
fractional number stays fractional. Reading `/state` and writing it back is a no-op.

**Validation happens at `t = 0`.** The expression is parsed when the state is accepted, so a
malformed one is a `400` with the offending text rather than a controller error on the first tick,
and a value that is negative or non-finite at `t = 0` is refused. A value that dips negative *later*
is a normal thing to write — a sinusoid crosses zero — and is clamped by the controller at runtime
(rates and utilizations clamp to zero; `cow_write_ratio` clamps to `1.0`).

