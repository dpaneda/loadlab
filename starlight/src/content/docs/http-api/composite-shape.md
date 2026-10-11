---
title: "The composite workload shape"
---

A full workload names several controllers at once, and writing it usually wants one request rather
than a round trip per object. The **composite shape** is any body that states more than one
top-level object:

```json
{
  "controller": { "max_memory_bytes": 536870912 },
  "memory": { "anonymous_rss": "500MiB + 200MiB*sin(t/30)" },
  "file_backed": { "rss": "200MiB" },
  "cpu":        { "utilization": "100 + 50*sin(t/10)" },
  "memory_access": { "pages_per_second": 10000 }
}
```

Accepted by `PUT /workload` and `PATCH /workload`, each of which could equally name just one of
these objects. No field has an alias: the canonical names above are the only names, and a body
never has to spell a controller it is not interested in.

Three properties matter:

- **Every controller defaults to generating nothing.** A body that names `memory` but not `cpu`
  leaves the CPU target at zero, and zero creates no worker threads at all. The fault rate is
  `memory_access.pages_per_second`, the page pattern `memory_access.pattern`.
- **The shape round-trips.** `GET /state` reports exactly the spelling that was accepted, and
  reading `/state` and writing it back is a no-op: there is no normalization to reverse.
- **A wrong type in a known field is a `400`; an unknown key is ignored.** `{"running": "yes"}`
  is refused because a boolean belongs there, but a key loadlab does not know is skipped rather
  than turning the whole request into an error. A typo in a field you meant to set will not fail
  loudly, so read `/state` back and compare.