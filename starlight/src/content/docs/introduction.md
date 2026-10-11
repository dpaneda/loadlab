---
title: "Introduction"
---

loadlab is a programmable Linux workload laboratory. It expresses a desired resource workload as
state, lets that state change over HTTP while the process runs, and converges toward it
continuously while reporting what the kernel actually measured.

Every tool that came before it measured the machine rather than the workload. `stress-ng` tells you
it allocated 4 GiB; it does not tell you that `sum(Rss)` across four processes says 16 GiB while
`sum(Pss)` says 4 GiB. loadlab is built to make that difference first-class.

The interesting part is the third clause of the sentence above. Every controller here is a *model*
of a mechanism (arena, mapping, page walk, block read) sitting next to an *observation* of the
result (`/proc/<pid>/smaps_rollup`, fault counters, `schedstat`). A target is never echoed back as
an achievement: `requested` and `actual` are separate fields everywhere, and an unconverged
controller says so rather than rounding up to the target.

## What this book covers

- **[Getting started](../getting-started/)** — build it, drive it with `curl`, read it back.
- **[Core concepts](../concepts/)** — the whole mental model in one page: controllers and
  expressions at a glance.
- **[Going further](../examples/)** — workloads that ramp, breathe and layer, all in expressions.
- **[Expressions](../expressions/)** — the full expression language: `t`, units, grammar, clamping.
- **[Controllers](../controllers/overview/)** — the catalog of what exists and what each one is
  for, then one page per controller.
- **[HTTP API](../http-api/authentication/)** — every endpoint, field and error code.
- **[Reference](../configuration/)** — the process flags, the host, the internal model, and how to
  deploy it.

The base URL throughout is `http://127.0.0.1:8080` unless a chapter says otherwise.
