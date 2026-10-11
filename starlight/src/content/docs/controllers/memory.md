---
title: "Anonymous memory"
---

### In short

`memory.anonymous_rss` targets the process's **total anonymous RSS** — the anonymous, non-file,
non-shared pages the kernel reports for the whole loadlab process. The controller grows and shrinks
an `mmap`ed arena so that the whole process sits at that number. It is the plainest of the memory
controllers: RAM, no file, no disk.

```json
{"memory":{"anonymous_rss":"512MiB"}}
```

What to watch in `GET /metrics` under `controller`: `last_target_arena_bytes`,
`arena_resident_bytes`, `convergence_error_bytes` and `converged`.

## What `anonymous_rss` means

The target is not "the arena holding 512 MiB". loadlab itself — process text, libc, the HTTP server,
the arena bookkeeping — also holds anonymous pages, so the arena is sized to make the **whole
process** report the requested anonymous RSS:

```text
desired_arena = anonymous_rss - baseline_anonymous_rss_at_startup
```

`loadlab_memory_baseline_rss_anon_bytes` is the anonymous RSS measured at startup, and
`loadlab_memory_rss_anon_bytes` is the whole process's current value, so the workload's own
contribution is visible as a delta rather than a claim. That silent subtraction is what keeps
`anonymous_rss: 0` from being unachievable — and what makes `loadlab_memory_baseline_rss_anon_bytes`
a useful number rather than trivia.

The arena target is then clamped by `--max-memory-bytes`, the process ceiling; see
[Memory ceiling](./memory-ceiling/).

## The arena

The arena is a set of `mmap(MAP_PRIVATE | MAP_ANONYMOUS)` regions of 64 MiB each, extended on
demand. Growing **touches one byte in every page**: the point is to turn virtual mappings into
resident pages and, in doing so, to create the minor-fault workload the memory-access controller
then walks. A target is virtual; residency is the thing that has a cost.

Shrinking releases the tail with `MADV_DONTNEED`, and chunks that end up completely empty are
**unmapped**, so the virtual address space comes back down as well as the resident memory. The
arena's resident bytes are known exactly from bookkeeping as well as measured:

| `controller` field | meaning |
| --- | --- |
| `last_target_arena_bytes` | what the controller last aimed for |
| `arena_resident_bytes` | what the arena holds right now |
| `convergence_error_bytes` | target minus actual, signed; positive means below target |
| `converged` | within `tolerance_bytes` of the target |

## Tolerance, not flapping

The arena only changes when it is off by more than `controller.tolerance_bytes` (default
2 MiB). Within tolerance, nothing moves; that is the hysteresis that keeps a control loop at a
steady target from grinding against its own measurement noise. `converged` is reported from the
same number, so a converged workload and a steady arena are the same statement.

The same tolerance window is what lets a slowly-moving expression be tracked without churn: a
`sin` target moves a little every tick, and the arena follows in steps rather than chasing every
partial-MiB wobble.

## Expressions and the run clock

`anonymous_rss` is a continuous magnitude: a number or an expression in `t`, following the
[expression contract](../../expressions/). `"256MiB + 128MiB*sin(t/60)"` makes the arena breathe.
A value that dips negative at runtime is clamped to zero (a sinusoid crossing zero is normal); a
negative value at `t = 0` is a `400`. Stopping the workload (or unsetting the field) converges the
arena to zero.

## THP applies here

`memory.thp` is the knob for exactly this arena, and the two pages belong together: the arena is
the anonymous region a transparent-huge-page policy asks the kernel to back. See
[Transparent huge pages](./thp/).