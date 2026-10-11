---
title: "Controllers: the catalog"
---

This is the menu. Each section below is one page in this book; every controller follows the same
recipe as every other — a desired target, a control loop, and a kernel observation measured through
`/proc` — so this page just answers "which one do I want?" and points at the deep dive.

| controller | what it is for | what it generates | key targets |
| --- | --- | --- | --- |
| [CPU utilization](./cpu/) | burn CPU time | worker threads spinning and sleeping in a duty cycle | `cpu.utilization`, `workers` |
| [Anonymous memory](./memory/) | hold RAM | an `mmap`-ed arena, grown and shrunk toward the target | `memory.anonymous_rss` |
| [Page access patterns](./page-access/) | generate page faults | a walk across the anonymous arena's pages | `memory_access.pattern`, `pages_per_second` |
| [File-backed memory](./file-backed/) | hold RAM **and** disk | a real file mapped `MAP_SHARED`, with its own fault loop | `file_backed.rss`, `pages_per_second` |
| [Shared memory](./shared/) | make copy-on-write visible | real processes mapping one shared segment plus private copies | `shared.shared_bytes`, `private_bytes`, `actors` |
| [Disk I/O](./io/) | move bytes on disk | reads, writes and `fsync` on a scratch file loadlab owns | `io.read_bps`, `write_bps`, `iops`, `fsync_rate` |
| [Network](./network/) | generate raw traffic | TCP or UDP connections and messages to one target | `network.connections_per_second`, `messages_per_second` |

Two more pages cover knobs that shape the whole process rather than one resource dimension:

- **[Memory ceiling](./memory-ceiling/)** — `controller.max_memory_bytes` clamps the anonymous
  arena to a per-process ceiling no state update (and no partial update that forgets it) can exceed.
- **[Transparent huge pages](./thp/)** — `memory.thp` asks the kernel to back the arena with huge
  pages, and reports what the host actually allowed.

## What every controller has in common

- **Requested and actual are separate fields.** `GET /metrics` reports what you asked for
  (`requested_*`) and what the kernel measured (`*_actual`). An unconverged controller says so.
- **Clamping beats failing.** A target above what the process can physically deliver is clamped and
  reported `saturated` — capable of an exact approach to the limit — while a target that breaks a
  *validation* rule (wrong type, unknown pattern, negative at `t=0`) is a `400` and changes nothing.
- **Targets are expressions in `t`** where every magnitude field accepts them; counts and choices
  stay literal (see [Expressions](../../expressions/)).
- **Each step is bounded by a process flag.** `--max-memory-bytes`, `--max-file-backed-bytes`,
  `--max-shared-bytes` and `--max-io-bytes` are the safety ceilings an operator fixes at startup, so
  an expression that grows without bound is capped by the host, not by hope.

The section header for each controller is the answer to "what is it for"; the page under it is the
answer to "how does it work and what does it refuse to fake."