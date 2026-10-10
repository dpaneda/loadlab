# File-backed memory

`file_backed.rss` targets memory backed by a file loadlab creates, maps `MAP_SHARED`, and writes
through. The distinction from `memory.anonymous_rss` is not cosmetic: an anonymous page is
memory the kernel had to find, while a file page comes out of the page cache, occupies a block
on disk, and can be reclaimed under pressure. Reproducing that is the reason this controller
exists.

The file is created with `O_EXCL` at mode 0600 and unlinked on clean termination — a normal exit
or a `SIGTERM`, which is what systemd sends. **A client can never name the file** — only the
directory it is created in comes from configuration, and loadlab still picks the name. The
default API must not overwrite arbitrary user paths, and a `PATCH` cannot smuggle one in.

A `SIGKILL` is the one case that leaves the file behind, because unlinking it on `Drop` is the
only cleanup mechanism there is: no handler runs, so nothing can remove it. This is inherent to
keeping the file visible for the operator to watch with `ls`, `du` and `iostat`, which is the
whole point of the feature. The leftover is bounded by `--max-file-backed-bytes` and named
`.tmp*` in the configured directory; a restart reuses no state and does not adopt or read it.

### The directory has to be a real filesystem

```bash
cargo run --release -- --file-backed-dir /var/lib/loadlab   # or LOADLAB_FILE_BACKED_DIR
```

The default is the current working directory, **not** `/tmp`. On most modern hosts `/tmp` is
tmpfs, and a file there is shared memory rather than file memory: the kernel accounts those pages
under `RssShmem`, there is no page cache and no writeback, and no disk is consumed. A
file-backed workload on tmpfs would report the residency it was asked for, charge no disk, and
exercise none of the behaviour above — while looking, to every metric in the system, exactly
like the real thing. Startup fails if the directory resolves to a memory-backed filesystem
rather than quietly generating the wrong phenomenon.

### Disk ceiling

```bash
cargo run --release -- --max-file-backed-bytes 268435456   # or LOADLAB_MAX_FILE_BACKED_BYTES
```

Defaults to 1 GiB, and is separate from `--max-memory-bytes` because file-backed memory costs
disk as well as RAM: every page paged in is a page the filesystem has allocated a block for. A
`file_backed.rss` above it is **clamped and reported as saturated**, the same way an
above-capacity CPU target is, because `rss` is an expression and only its evaluated value can be
compared. `requested_rss_bytes` reports what was asked for, `target_rss_bytes` what was applied.

A `file_backed.rss` above `controller.max_memory_bytes` is different, and is rejected with `400`:
that one is desired-state validation and does not depend on evaluating anything.

#### The ceiling can be overshot by up to one chunk

The mapping is extended in 8 MiB chunks, so the file's length is always a multiple of 8 MiB and
**can exceed `--max-file-backed-bytes` by up to 8 MiB**. A 32 MiB ceiling therefore produces a file
of 32 MiB for a 32 MiB target, and up to 40 MiB for a target just under the ceiling. This is
deliberate: the alternative is mapping a length that is not chunk-aligned, which means `ftruncate`
mid-mapping and a `SIGBUS` for any page of the tail that has not been written back.

The arithmetic saturates instead of wrapping, so a ceiling near `u64::MAX` is applied as the
largest representable value rather than becoming a small one. `file_bytes` is authoritative and
`loadlab:file_backed_ceiling_ratio` in the alerting rules divides by the same ceiling the
controller used, so a ratio above 1.0 is the expected shape of a partially-filled last chunk
rather than a sign of a bug.

### What the controller reports

Under `controller.file_backed` in `GET /metrics`:

| field | meaning |
| --- | --- |
| `path` | the file loadlab created, or `""` when none has been needed yet |
| `file_bytes` | current file length; never shrinks below a live mapping |
| `pages` | resident pages |
| `rss_bytes` | `pages * page_size` |
| `requested_rss_bytes` | the evaluated target before the disk ceiling |
| `target_rss_bytes` | what was applied after clamping |
| `process_rss_file_bytes` | process-wide `RssFile` from procfs, including the executable |
| `actual_pages_per_second` | measured fault rate |
| `minor_faults_rate` / `major_faults_rate` | the kernel's own counters, split |
| `converged` | within tolerance of both the residency and the rate target |
| `saturated` | the request exceeded the disk ceiling, or the rate exceeded what one pass can produce |

`rss_bytes` and `process_rss_file_bytes` are deliberately both present and are **not** two
readings of the same thing. The first is the controller's own mapping; the second is everything
the process has in file-backed pages, including its own text segment. A difference between them
is normal and is usually loadlab's executable.

The fault rates are process-wide, so they are an **upper bound** on what this controller caused.
An anonymous arena under load at the same time contributes to the same counters, and
`converged` should be read with that in mind. [Whose faults](#whose-faults) splits that total
per thread.

### Whose faults

Under `controller.memory_access` in `GET /metrics`, the process-wide fault counters are
apportioned between the thread that generates loadlab's memory pressure and everything else:

| field | meaning |
| --- | --- |
| `attribution_available` | whether both sides of the split could be read |
| `generated_minor_faults` / `generated_major_faults` | faults taken by the controller thread |
| `incidental_minor_faults` / `incidental_major_faults` | faults taken by every other thread |

`generated + incidental` equals `minor_faults_total` / `major_faults_total` exactly. That is
by construction, not luck: the process counters and the controller thread's own are sampled
once, together, and the halves are apportioned from that single sample.

The sampling order matters and is not the obvious one. The controller thread faults
continuously while these figures are being read, and the process-wide aggregate in
`/proc/<pid>/stat` is not read atomically with the per-thread counter — so reading the total
first reliably produces a thread count *above* the total it is supposed to be a part of. The
controller's share is therefore clamped to the total rather than subtracted from it. On a tick
where the controller faults faster than the aggregate refreshes, one fault lands in
`incidental` instead of `generated`; the alternative is letting the two halves disagree, which
corrupts every rate derived from them by an unbounded amount.

`incidental` is not noise to be dismissed. It is the async runtime, the allocator and the HTTP
server doing their own work, and a non-zero value while a workload runs is normal. It is worth
watching as its own series: incidental minor faults climbing under load say that something
outside the controllers is allocating, which no other signal here would show.

`attribution_available` is `false` only if the controller thread's counters cannot be read at
all. The split is then left at zero rather than reporting the whole process as `incidental`,
which would say the opposite of what it means.

### The file outlives the workload

Stopping sets the target to zero, which releases the pages, but the file and its mapping are kept
for the life of the process. This is deliberate: the mapping is never remapped, so a workload
that cycles up and down does not pay a re-map — and the re-map would drop every resident page —
each time. The operator ceiling is what bounds the file.

It also means the file is never truncated shorter than its high-water length. Truncating a file
under a live mapping raises `SIGBUS` on the next access, which would kill the process rather
than return an error.

