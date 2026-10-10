# The host

`GET /metrics` and `GET /prometheus` both describe the machine the numbers came from. Read once at
startup; a failure is a startup failure rather than a degraded scrape, because a measurement that
cannot say where it was taken is one nobody can reason about later.

| Field | Meaning |
| --- | --- |
| `kernel_name` / `kernel_release` / `kernel_version` | `uname(2)` |
| `machine` | `uname` machine, e.g. `x86_64` |
| `cpus` | logical CPUs visible to the process |
| `cpu_model` | first `model name` in `/proc/cpuinfo`, or `null` |
| `memory_total_bytes` | `MemTotal` from `/proc/meminfo`, or `null` |
| `page_size_bytes` | `sysconf(_SC_PAGESIZE)` |

`cpu_model` and `memory_total_bytes` are present-or-null rather than always present: the parser is
allowed to find nothing, and the key must not vanish, so a client can tell "no model" from "an older
loadlab".

cgroup limits are deliberately not included. The numbers here would be the machine's, while the
workload is confined to something smaller, and publishing both without a clear relationship between
them invites a comparison that is wrong. That is a later question, and it wants an answer rather
than a field.

