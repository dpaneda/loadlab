---
title: "loadlab architecture"
---

## 1. Core model

loadlab has three layers:

1. **Desired state** — what the user wants.
2. **Controllers** — mechanisms that try to reach the desired state.
3. **Observation** — what Linux says actually happened.

Never collapse these layers into one abstraction.

```text
              HTTP
               |
               v
        +---------------+
        | Desired State |
        +-------+-------+
                |
                v
        +---------------+
        |   Controller  |
        +-------+-------+
                |
        v       v       v
      mmap    workers  syscalls
        |
        v
      Linux
        |
        v
      /proc
```

## 2. Concurrency model

The HTTP server runs on Tokio's multi-threaded runtime.

The resource controller runs on one dedicated OS thread. This is intentional: the memory arena contains raw pointers and should have a single clear owner. The HTTP layer never receives a reference to the arena and never manipulates mappings directly.

Shared state is protected by `std::sync::RwLock` because reads/writes are tiny and synchronous. Do not hold the lock while calling `mmap`, `madvise`, filesystem APIs, or other potentially slow operations.

## 3. Hot configuration

An HTTP update replaces or merges the desired state. The controller observes the new state on its next tick.

There is no request-to-worker callback path. This is deliberate: a periodic reconciliation loop keeps the system deterministic and makes feedback control easier to reason about.

## 4. Expression compilation

The HTTP layer validates expression syntax before accepting configuration.

The controller caches the compiled expression and recompiles only when the source string changes.

Expression evaluation must be side-effect free. It is data, not code.

## 5. Memory controller

`AnonymousArena` owns a set of anonymous `mmap` regions. It tracks logical resident bytes independently of `/proc` observations.

Growth:

1. Ensure enough virtual mapping capacity.
2. Touch one byte per page.
3. Record the new logical resident size.

Shrink:

1. Apply `MADV_DONTNEED` to the tail.
2. Drop empty trailing mappings.
3. Record the logical resident size.

The process is therefore deliberately generating memory pressure/page activity instead of asking an allocator to grow an opaque heap.

## 6. Procfs observation

`/proc/self/smaps_rollup` is the preferred source for detailed aggregate mapping statistics. `/proc/self/status` is used for `RssAnon`, `RssFile`, and `RssShmem`. `/proc/self/stat` supplies minor/major fault counters.

Linux documents `smaps_rollup` as an aggregate of mapping-level `smaps` information, while noting that RSS values in scalable `/proc` interfaces can have different precision/cost trade-offs. The implementation should preserve the distinction between fast counters and detailed snapshots.

### Per-thread observation

Two measurements need per-thread accounting rather than a process total, and both come from
`/proc/self/task/<tid>`:

- `schedstat`, which reports how long each thread was on a CPU, how long it was runnable but
  not running, and how many times it was scheduled. It requires `CONFIG_SCHEDSTATS`.
- the `minflt`/`majflt` fields of `stat`, which is the same split by thread that the
  process-wide counters already provide.

`src/procfs.rs` reads these files and nothing else; it does not interpret them. `src/observation.rs`
turns them into `WorkerScheduling` and `FaultAttribution`, which are plain data with an
`available` flag. Keeping interpretation out of `procfs` is what leaves room for a second source
later: an eBPF backend would have to produce those two values, and nothing else in loadlab would
have to change.

Two rules hold across both sources:

- **a missing measurement is not a zero.** Every field that can fail to be read sits behind
  `available`, so a kernel without `CONFIG_SCHEDSTATS` reports "unavailable" rather than a
  plausible-looking pool that never waited for anything.
- **a derived figure is computed from one sample, not from re-reads.** The controller thread
  faults continuously while it is being measured, so reading a process total and a thread total
  separately produces halves that do not add up to the whole. Figures that are compared are read
  once and apportioned from that single sample.

## 7. The experiment clock and the run's identity

`src/experiment.rs` owns three things that have nothing to do with any one controller: what `t` means,
how one seed becomes three independent ones, and how an RFC 3339 string becomes an instant.

**The clock is anchored, not sampled.** `ExperimentClock` holds a monotonic `Instant` origin and an
optional offset from a configured `start_time`. It re-anchors when the workload starts and when
`start_time` changes, and otherwise advances from `Instant::elapsed`. The wall clock is read exactly
once per anchor, at the moment the origin moves. Two reasons:

- reading `SystemTime` every tick would make `t` step backwards when NTP adjusts the clock, and every
  expression in the workload would follow it;
- the offset is enough. Once the distance between "now" and the configured instant is known, monotonic
  time carries `t` from there without the wall clock being involved again.

`observe` returns `Observed::{Unchanged, Reanchored, Rejected}` rather than a bare `bool` so the
caller can log a timestamp it could not parse *once* instead of once per tick — at a 1ms tick, once
per tick is a thousand identical log lines a second. A rejected timestamp still leaves a working
clock: the run continues measured from its start, which is a claim loadlab can support, rather than
refusing to run.

**One seed, three walks.** `stream_seed` mixes the experiment seed with a per-stream domain constant
through SplitMix64's finalizer. Seeds `1`, `2` and `3` given to three controllers must not produce
walks a fixed distance apart, or patterns would be visibly correlated across runs and two controllers
would look related in a recording. A controller's own `random_seed` bypasses the mixer entirely,
because a scenario written against a specific walk has to keep producing that walk.

**The RFC 3339 parser is hand-written** and so is the host reader's date-free arithmetic. The
alternative is a date-time dependency for one field, and a workload generator that pulls in a calendar
library to move a monotonic clock is a dependency nobody asked for. The parser accepts what RFC 3339
requires for a wall-clock instant — `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)` — and rejects the rest,
including leap seconds, because there is no honest way to place `:60` on a monotonic timeline.

**Identity is published, not stored twice.** `experiment` in the desired state is what a client asked
for; `experiment` in `/metrics` is what the controller actually did with it. They are separate because
they can differ: a requested `start_time` the controller could not parse leaves the desired state
unchanged and the observation anchored elsewhere, and a recording that conflated the two would report
the intent as the result.

**Host metadata is read once, at startup.** It cannot change while loadlab runs, and a failure to read
it is a startup failure rather than a degraded scrape — a measurement that cannot say where it was
taken is one nobody can reason about later. The fields are the machine's, not the workload's
confinement's, which is why cgroup limits are absent rather than half-present.

## 8. Disk I/O: descriptors and alignment

The I/O controller holds two descriptors on the same file. One is the `NamedTempFile`, which owns
the path and unlinks it on drop; `tempfile` offers no way to hand it custom open flags. The other is
opened separately and is the one every `pread`, `pwrite` and `sync_data` goes through, because
`O_DIRECT` has to be in the `open(2)` call itself. Both name the same inode, so `set_len` on one is
visible through the other. The descriptor is reopened only when `direct` changes, not per tick.

`O_DIRECT` requires the buffer address, the file offset and the length to be aligned to the
filesystem's logical block size. loadlab satisfies this unconditionally: offsets and lengths are
whole multiples of `block_size`, which configuration validation requires to be a multiple of 4096
when `direct` is on, and buffers come from an over-allocated `Box<[u8]>` whose sub-slice starts at
the first 4096-aligned address. The `Box` matters — a `Vec` could reallocate and silently leave the
buffer unaligned.

This is treated as a contract with the filesystem rather than something to probe at runtime. There is
no portable ioctl for the required alignment, `BLKSSZGET` reports the device's block size rather
than the constraint that binds first, and whether the kernel rejects a misaligned direct read is not
stable across versions. Probing for it would produce a test that passes on a lenient filesystem and
fails on a strict one, so the alignment is simply always satisfied.

## 9. Failure policy

Configuration errors are rejected synchronously by the HTTP API.

Runtime errors do not crash loadlab. They are recorded in controller status and logged, so the process remains inspectable.

One runtime error is deliberately *not* downgraded to a degradation: a failed `open` with
`O_DIRECT` stops the I/O controller rather than falling back to buffered I/O. Where a missing
observation is satisfied by continuing (`CONFIG_SCHEDSTATS` reports `available = false`), a missing
workload is satisfied by refusing to run a different one. Generating page-cache I/O under a
`direct: true` label would be worse than reporting nothing, because the number would look right.
