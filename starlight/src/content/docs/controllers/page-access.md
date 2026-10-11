---
title: "Page access patterns"
---

> **In short.** `memory_access.pages_per_second` walks the anonymous arena's pages with a target
> fault rate, shaping *how* memory is touched — not how much of it is resident, which the memory
> controller owns. `0` (the default) means no walk at all. The same patterns drive the file-backed
> controller. Minimal form:
>
> ```json
> {"memory_access":{"pattern":"sequential","pages_per_second":5000}}
> ```
>
> What to watch in `GET /metrics` under `controller.memory_access`: the requested and actual fault
> rates, plus `converged`.

`memory_access.pattern` and `file_backed.access_pattern` share one vocabulary. They differ
in *what* is being faulted, not in how it is walked, so the same pattern name means the same
traversal in both. The two names spell the same idea differently because they are separate
controllers with their own state: `memory_access` calls it `pattern`, `file_backed` calls it
`access_pattern`.

| pattern | traversal |
| --- | --- |
| `sequential` | front to back, wrapping |
| `random` | uniform over the resident set, from a seeded LCG so a run reproduces |
| `stride` | every `stride_pages`-th page |
| `working_set` | a fixed window of `working_set_pages` pages, cycled, so the rest of the mapping goes cold |
| `hot_cold` | a small hot set touched every pass and a large cold set touched rarely |
| `cold_scan` | one pass over the cold region, then a long dwell |

The default is `sequential`. A pattern that needs a size takes it from the same-named field
(`working_set_pages`, `stride_pages`); leaving it unset uses a size that keeps the pattern's
shape visible without allocating anything unexpected.

`pages_per_second` of `0` means no access loop at all, which is the default for both controllers.
That is the only rate at which `converged` does not depend on the measured fault rate: the
counters behind it are process-wide, so at a zero target the API server, a metrics scrape, and
any other load in the process all register as faults this controller is being charged for. A
zero target reports converged because nothing was asked for.

