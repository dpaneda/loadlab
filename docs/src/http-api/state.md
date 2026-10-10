# GET /state

Returns the full desired workload state.

```json
{
  "running": true,
  "controller": {
    "tick_ms": 100,
    "tolerance_bytes": 2097152,
    "max_memory_bytes": 8589934592
  },
  "memory": {
    "anonymous_rss": "512MiB + 256MiB*sin(t/10)",
    "thp": "madvise"
  },
  "cpu": {
    "utilization": "150 + 50*sin(t/30)",
    "workers": 2,
    "cpus": [],
    "affinity": "auto"
  },
  "memory_access": {
    "access_pattern": "sequential",
    "pages_per_second": 10000
  },
  "file_backed": {
    "rss": "128MiB + 64MiB*sin(t/10)",
    "access_pattern": "random",
    "pages_per_second": 2000,
    "madvise": "sequential"
  },
  "shared": {
    "shared_bytes": "512MiB",
    "private_bytes": "128MiB",
    "backing": "memfd",
    "cow_write_ratio": 0.5,
    "actors": 4
  },
  "io": {
    "read_bps": 8388608,
    "write_bps": 0,
    "iops": 0,
    "fsync_rate": 0,
    "block_size": 4096,
    "queue_depth": 8,
    "pattern": "sequential"
  },
  "network": {
    "transport": "tcp",
    "target": "127.0.0.1:9000",
    "concurrency": 4,
    "connections_per_second": 10.0,
    "messages_per_second": 5000.0,
    "message_size": 1024
  },
  "experiment": {
    "seed": 42,
    "start_time": null
  }
}
```

Every field is optional, and every one of them defaults to generating nothing:

| field | default | meaning |
| --- | --- | --- |
| `running` | `false` | whether the workload should generate anything at all |
| `controller.tick_ms` | `100` | control loop interval |
| `controller.tolerance_bytes` | `2097152` | how far memory may sit from its target and still count as converged |
| `controller.max_memory_bytes` | clamped | requested ceiling; see [Memory ceiling](../controllers/memory-ceiling.md#memory-ceiling) |
| `memory.anonymous_rss` | none | number or expression, in bytes |
| `memory.thp` | `"default"` | transparent huge page policy for the anonymous arena: `"default"`, `"always"`, `"madvise"` or `"never"`; see [Transparent huge pages](../controllers/thp.md#transparent-huge-pages) |
| `memory_access.access_pattern` | `"sequential"` | which pages the fault loop walks |
| `memory_access.pages_per_second` | `0.0` | number or expression in `t`; target fault rate against the anonymous arena |
| `memory_access.random_seed` | `1` | seed for the `random` pattern, so a run is reproducible |
| `file_backed.rss` | none | number or expression, in bytes of file-backed memory |
| `file_backed.access_pattern` | `"sequential"` | which pages the file-backed fault loop walks |
| `file_backed.pages_per_second` | `0.0` | number or expression in `t`; target fault rate against the file |
| `file_backed.madvise` | `"random"` | readahead policy; see [File-backed memory](../controllers/file-backed.md#file-backed-memory) |
| `file_backed.working_set_pages` | `1024` | pages the `working_set` pattern cycles through |
| `file_backed.stride_pages` | `1` | step for the `stride` pattern |
| `file_backed.random_seed` | `1` | seed for the `random` pattern, so a run is reproducible |
| `shared.shared_bytes` | none | number or expression, in bytes; the one segment every actor maps `MAP_SHARED` |
| `shared.private_bytes` | none | number or expression, in bytes per actor; the segment each actor maps `MAP_PRIVATE` and writes |
| `shared.backing` | `"memfd"` | kernel object: `"memfd"` or `"sysv"`; see [Shared memory](../controllers/shared.md#shared-memory-and-copy-on-write) |
| `shared.cow_write_ratio` | `0.0` | number or expression in `t`; fraction of each private segment to write, from `0.0` to `1.0` |
| `shared.actors` | `1` | actors, including loadlab itself; each actor past the first is a real process |
| `io.read_bps` | `0.0` | number or expression, in bytes per second; target read throughput |
| `io.write_bps` | `0.0` | number or expression, in bytes per second; target write throughput |
| `io.iops` | `0.0` | number or expression in `t`; cap on total read plus write operations per second; `0.0` means uncapped |
| `io.fsync_rate` | `0.0` | number or expression in `t`; `fsync`/`fdatasync` calls per second; independent of reads and writes |
| `io.block_size` | `4096` | block size for every operation, and the granularity of the region |
| `io.direct` | `false` | open the file with `O_DIRECT`, bypassing the page cache |
| `io.queue_depth` | `1` | maximum in-flight operations per tick; `1` is strictly serial |
| `io.pattern` | `"sequential"` | which blocks the walk visits: `"sequential"`, `"random"` or `"stride"` |
| `io.stride_blocks` | none | for `"stride"`, the distance between blocks |
| `io.random_seed` | none | for `"random"`, the seed, so a replay walks the same blocks |
| `network.transport` | `"tcp"` | transport protocol: `"tcp"` or `"udp"` |
| `network.target` | `""` | destination `host:port`; empty generates nothing |
| `network.concurrency` | `1` | concurrent worker slots / connection pool size |
| `network.connections_per_second` | `0.0` | number or expression in `t`; fresh TCP connections to open per second |
| `network.messages_per_second` | `0.0` | number or expression in `t`; messages to send per second over persistent connections or UDP |
| `network.message_size` | `1024` | bytes per message (1..=1048576) |
| `cpu.utilization` | `0.0` | number or expression in `t`, in percent of a logical CPU |
| `cpu.workers` | `0` | worker threads; `0` means one per selected CPU |
| `cpu.cpus` | `[]` | logical CPU IDs to use; `[]` means all of them |
| `cpu.affinity` | `"auto"` | `"auto"` or `"pin"` |
| `experiment.seed` | none | fallback seed for every controller without its own `random_seed`; see [The experiment](../experiment.md#the-experiment) |
| `experiment.start_time` | none | RFC 3339 instant to treat as `t = 0`; see [The experiment](../experiment.md#the-experiment) |

`experiment` follows the same patch rule as everything else: a `PATCH` that does not mention it
leaves the seed and start time alone, and a `PATCH` that does mention it starts both from their
defaults, which means it clears them. To change one without the other, name both.

A `PATCH` that does not mention a field leaves it alone; a `PATCH` that does mention one starts
from the field's default. That is what makes the CPU defaults load-bearing: a patch that only
touches memory restores `cpu.utilization` to `0`, and `0` creates no worker threads at all.

The same rule applies to `file_backed`: a `PATCH` that only touches `memory` restores
`file_backed.rss` to none, which releases the file-backed pages. It applies to `shared` too: a
patch that does not mention it restores the sizes to none, which signals the actors and releases
the segment. `io` is no different: a patch that does not mention it restores every rate to zero,
which stops the disk walk and releases nothing until the file is dropped. `network` follows the
exact same rule: omitting it leaves existing network settings alone, while a patch that touches
it without specifying a target defaults target to `""`, generating no traffic.

