# The composite scenario shape

A full workload is long: five or six top-level objects, each with a dozen fields. The **composite
scenario shape** is a shorter spelling of the same thing, for the fields a scenario is normally
written in terms of:

```json
{
  "controller": { "max_memory_bytes": 536870912 },
  "anonymous":  { "rss": "500MiB + 200MiB*sin(t/30)" },
  "file_backed": { "rss": "200MiB" },
  "cpu":        { "utilization": "100 + 50*sin(t/10)" },
  "faults":     { "minor_rate": 10000 }
}
```

Accepted by `PUT /workload`, `PATCH /workload` and `POST /scenarios/load`. Two aliases do the work:

| Alias | Means |
| --- | --- |
| `anonymous.rss` | `memory.anonymous_rss` |
| `faults.minor_rate` | `memory_access.pages_per_second` |

`file_backed` and `cpu` are already top-level objects, so they are spelled the same way in both
shapes.

Three properties matter:

- **An alias is accepted on input and never appears on output.** `GET /state` keeps reporting
  `memory.anonymous_rss` and `memory_access.pages_per_second`, so a client that only knows the
  canonical spelling is unaffected. The aliases are folded in before the state is stored, and
  neither key is ever echoed back.
- **The alias wins.** If a body carries both `anonymous.rss` and `memory.anonymous_rss`, the alias
  is the one applied. This matters for `PATCH`, which merges into the existing state: patching the
  alias onto a state that already carries the canonical field replaces it, so a client can move
  between the two spellings without clearing the other one first.
- **An unknown field is a `400`.** Both alias objects are closed. `faults.major_rate` in particular
  is refused with an error naming `minor_rate` as the accepted field: this API has no major-fault
  controller, and reporting success for a request that does nothing is the one outcome it never
  produces.

