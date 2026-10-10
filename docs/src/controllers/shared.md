# Shared memory and copy-on-write

`shared.shared_bytes` is a single segment that every actor maps `MAP_SHARED`, and
`shared.private_bytes` is a second segment that each actor maps `MAP_PRIVATE` and writes to. The
two together are the point of this controller: the shared segment is one copy of physical memory
no matter how many actors map it, so summing `Rss` over the actors counts it once per mapper, while
summing `Pss` divides it between them. This controller exists to make that difference first-class
rather than a footnote.

`shared.actors` counts the loadlab process itself as the first actor. `actors: 1` is a shared
segment nothing else maps and shows no overcount; `actors: 4` forks three real processes, each of
which re-executes loadlab in actor mode, maps the same object, and holds it until it is signalled.
A larger pool is clamped to `MAX_ACTORS` (64).

### How the actors are created

An actor is a forked child that re-executes the loadlab binary with an actor flag, rather than a
raw `fork()` from the threaded control process. `fork()` without `exec` in a process that already
has a Tokio runtime and an axum server running duplicates the whole address space and every lock it
holds, so the child is not guaranteed to be able to do anything useful. Re-exec costs one process
spawn per actor and is correct, which is the right trade for a tool whose numbers people size
machines with.

A memfd segment is handed to the child as an inherited file descriptor; a System V segment is
handed over by ID. Either way the child maps it `MAP_SHARED`, maps its private view, writes the
fraction asked for, and signals readiness over a pipe before the parent counts it. The readiness
handshake is what keeps the first tick's accounting free of a race: a child that has not mapped yet
would otherwise be counted as an actor holding no memory.

### Backing object: memfd or sysv

```bash
cargo run --release -- --shared-backing sysv   # or LOADLAB_SHARED_BACKING
```

Both are accounted as `RssShmem` and both expose the double-count. `memfd` is the default because
it is the ordinary path: a file descriptor that can be mapped `MAP_PRIVATE` to give a private view
whose pages fault from the shared object, which is what a real copy-on-write consumer looks like. A
System V segment has no such file descriptor, so its private view is anonymous rather than a
private mapping of the segment: it measures the cost of the writes without the fault-from-shared
behaviour. `sysv` is offered because it is visible in `/proc/sysvipc/shm` and some tooling watches
that, not because it is a better model of the phenomenon.

### The private view is bounded by the shared segment

`private_bytes` is clamped to `shared_bytes` for every actor. A private view copies pages *of the
shared segment*, so asking for more private bytes than the segment has is not a larger experiment,
it is a request the mapping cannot satisfy: a memfd `MAP_PRIVATE` mapping longer than its file
`SIGBUS`es on the first access past the end. The controller maps no more private bytes than there
are shared bytes to copy.

### Combined ceiling

```bash
cargo run --release -- --max-shared-bytes 2147483648   # or LOADLAB_MAX_SHARED_BYTES
```

Defaults to 2 GiB and bounds the **combined** footprint of the shared segment and every actor's
copy-on-write, because both are charged against the same physical memory on the same host. The
segment is one fixed cost; what it leaves is the copy-on-write budget:

```text
permitted_cow_bytes = max_shared_bytes - shared_bytes
```

which is then divided across the actors and the write ratio to give each actor's private view. A
request the ceiling cannot satisfy is **clamped and reported as saturated**, the same way a
file-backed or CPU request is. `requested_cow_bytes` reports what was asked for,
`target_cow_bytes` what was applied, and `private_bytes_per_actor` the effective per-actor view.

### What the controller reports

Under `controller.shared` in `GET /metrics`:

| field | meaning |
| --- | --- |
| `backing` | `"memfd"`, `"sysv"`, or `""` when no segment exists |
| `shared_bytes` | size of the shared segment, fixed at creation |
| `private_bytes_per_actor` | effective private view after clamping |
| `cow_write_ratio` | configured fraction of each private segment to write |
| `actors` | live actors, including loadlab itself |
| `child_actors` | live forked actors, i.e. `actors - 1` |
| `shared_physical_bytes` | the segment counted once |
| `shared_rss_bytes` | `sum(RssShmem)` over every actor, i.e. the segment once per mapper |
| `shared_overcount_bytes` | `shared_rss_bytes - shared_physical_bytes` |
| `cow_physical_bytes` | physical pages of copy-on-write across all actors |
| `accounted_bytes` | `sum(Rss)` over every actor, what naive accounting reports |
| `physical_bytes` | `sum(Pss)` over every actor, what the memory actually costs |
| `requested_cow_bytes` | evaluated copy-on-write request before the ceiling |
| `target_cow_bytes` | what was applied after clamping |
| `converged` | whether observed copy-on-write is within tolerance of the target |
| `saturated` | the request exceeded the combined ceiling |
| `actor_accounting` | per-actor `pid`, `rss_bytes`, `pss_bytes`, `is_controller` |

`shared_overcount_bytes` is deliberately **not** `accounted_bytes - physical_bytes`. That
difference contains every other page the actors have in common as well — the executable's text and
libc are the same physical pages in all of them — so it is nonzero even for a single actor. The
narrow quantity, `sum(RssShmem) - shared_physical_bytes`, is the double-count this controller is
responsible for and nothing else.

The convergence check is monotonic: `converged` is true once observed copy-on-write reaches the
target within tolerance. Overshooting is not non-convergence — an actor asked to write half its
private view that writes slightly more is at the target, not past an error — which is why the test
is `observed + tolerance >= target` rather than a symmetric comparison.

An actor that exits is reaped and replaced on the next tick rather than reported as an error: a
workload that stops being the workload that was asked for is a shortage visible in `actors`, not a
failed controller.

### Stopping

Stopping sets the shared sizes to zero, which signals every actor, waits for it, and releases the
segment. Actors are torn down before the segment: a System V segment must outlive every actor
mapped to it. A `Drop` runs the same path, so a stopped or panicking process does not leak actors
or leave a System V segment behind. A `SIGKILL` still cannot run `Drop`; a leftover memfd goes away
with the process, and a leftover System V segment is the one case that can persist.

