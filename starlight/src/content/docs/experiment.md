---
title: "The experiment"
---

Every workload carries an `experiment` block, and every observation carries the same identity. It
exists because a measurement of "4 workers at 400%" is not a fact on its own: it is a fact *of a run
on a host*, and without the second half of that sentence two numbers cannot be told apart from one
reproduction and one coincidence.

```json
"experiment": {
  "seed": 42,
  "start_time": "2026-03-01T09:00:00Z",
  "elapsed_seconds": 512.4,
  "start_offset_seconds": -1800.0,
  "active_scenario": { "name": "ramp", "version": "2026-03-01" }
}
```

| Field | Meaning |
| --- | --- |
| `seed` | the seed in effect, or `null` when none is configured |
| `start_time` | the configured `t = 0`, or `null` when `t` is the age of the run |
| `elapsed_seconds` | seconds since `t = 0`; what the workload's expressions are evaluated against |
| `start_offset_seconds` | seconds between `start_time` and the moment the run was anchored |
| `active_scenario` | `{name, version}` of the driving scenario, or `null` |

### Seed

`experiment.seed` is the fallback for every controller that does not set its own `random_seed`:

| Stream | Controller field it feeds |
| --- | --- |
| memory access | `memory_access.random_seed` |
| file-backed memory | `file_backed.random_seed` |
| disk I/O | `io.random_seed` |

A controller's own `random_seed` wins. That is what keeps a scenario written against a specific walk
producing the same walk, and it is why the experiment seed is documented as a fallback rather than an
override. The three streams are separated by domain constants before mixing, so seeds `1`, `2` and
`3` do not produce walks a fixed distance apart — without that, patterns would be visibly correlated
across runs and two controllers would appear related in a recording.

Same seed, same workload and same `start_time` means the same walk. Same seed and a *different*
`start_time` does not: the seed fixes the pattern, the start time fixes when in it the run is.

### `start_time` and the time base

`start_time` is an RFC 3339 timestamp and is nullable. It answers "when is `t = 0`", which is a
question a shared timeline has to answer once rather than once per host.

- **`null`** — the default. `t` is the age of the run: zero when the workload starts, and zero again
  on every restart. A restart partway through means the workload begins again from the beginning.
- **A timestamp** — `t` is measured against that instant, so two hosts running the same scenario at
  different moments agree on what `t` is. `t` keeps advancing while the workload is stopped, which is
  what makes a scenario with a deliberate gap in it expressible.
- **A future timestamp** — accepted. `t` is negative until the instant arrives. Refusing it would
  make a scenario shared across hosts uninstallable before its scheduled start, and clamping to zero
  would throw away the distance to the start rather than reporting it.

`start_offset_seconds` is what makes `elapsed_seconds` readable. With an offset of `-1800` and an
`elapsed_seconds` of `512.4`, the observation was taken five minutes *before* the experiment rather
than five minutes into it. It is absent when there is no `start_time`, because there is no instant
the offset could be measured from.

The clock does not move on every tick. It re-anchors when the workload starts and when `start_time`
changes, and otherwise advances from a monotonic origin. Reading the wall clock every tick would make
`t` depend on NTP steps for no benefit.

### Active scenario

`active_scenario` is `{name, version}` or `null`. The version is the point: a name alone cannot
distinguish a run from a later edit to the same name. It is `null` for a workload installed directly
through `PUT`/`PATCH /workload`, which is a different kind of run rather than an older one, and it is
resolved against the scenario store on every scrape rather than cached, so a marker that no longer
describes the running workload cannot be published.

