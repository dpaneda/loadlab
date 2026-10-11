---
title: "Transparent huge pages"
---

> **In short.** `memory.thp` asks the kernel to back the anonymous arena with transparent huge
> pages (`"default"`, `"madvise"`, `"always"` or `"never"`). It is a request, not a guarantee —
> the host has the last word — and the honest truth is the measured `AnonHugePages`, not the
> policy string.

`memory.thp` asks the kernel to back the anonymous arena with transparent huge pages (THP). It
takes four values and defaults to `"default"`:

| value | what loadlab does |
| --- | --- |
| `"default"` | nothing: the arena inherits whatever the host does process-wide |
| `"madvise"` | `MADV_HUGEPAGE` on the arena, and clears the process-wide THP disable |
| `"always"` | the same as `"madvise"` — see below |
| `"never"` | `MADV_NOHUGEPAGE` on the arena, and sets the process-wide THP disable |

`"always"` and `"madvise"` mean the same thing to loadlab. A process cannot change the host's
global mode without root, so both values ask for huge pages on this arena; the two names exist
because the API mirrors the host's own vocabulary, and an operator picks whichever matches the
setting read from `/sys/kernel/mm/transparent_hugepage/enabled`.

This is a **request, not a guarantee**. The host may be `never`, in which case only root can
change the outcome; `GET /metrics` reports what was asked for alongside what the host allows, and
the effective truth is always the measured value:

```json
{"controller":{"thp":{"requested":"madvise","host":"always","host_enabled":true}}}
```

`host` is `always`, `madvise`, `never` or `unavailable` (the knob is missing, for example on a
kernel without THP). The observed `AnonHugePages`, `loadlab_memory_anon_huge_pages_bytes` in the
Prometheus output, is the only number that says whether huge pages actually happened.

Runtime changes are not retroactive. Setting the policy applies the corresponding advice to the
arena's existing mappings and to any mapping created afterwards, but:

- returning to `"default"` clears the process-wide disable while leaving the advice already
  placed on existing regions — they keep it until the arena drops and recreates them;
- switching from `"never"` to `"always"` does not eagerly promote pages that were already faulted
  in as 4 KiB; `khugepaged` may collapse them later, on its own schedule.

Reserved hugetlbfs pages (`hugetlb`) are deliberately not part of this knob: those are a host
provisioning decision, not per-process advice.

