# Disk I/O

`io.read_bps` and `io.write_bps` generate reads and writes against a temporary file loadlab
creates and owns, and `io.fsync_rate` adds durability traffic on top. This is the controller that
turns loadlab from a memory tool into a disk one: the file-backed controller measures how much
memory the kernel keeps resident, while this one measures how fast a filesystem actually moves
bytes, including the writeback that residency never touches.

The file is created at mode 0600 in the directory given to `--file-backed-dir`, is written with a
non-zero pattern rather than left sparse (a hole would be satisfied without touching a block), and
is unlinked on `Drop`. **A client can never name the file or the directory.** The region is
block-aligned and bounded by `--max-io-bytes`.

### Rates are measured, not asserted

`loadlab_io_read_bps_actual` is derived from the bytes `pread` actually returned, and
`loadlab_io_write_bps_actual` from the bytes `pwrite` actually stored, divided by the tick's real
elapsed time. Neither is the rate that was issued. A filesystem that cannot keep up therefore shows
a persistent gap between the requested and the actual value instead of a controller that reports
success while moving less — which is the whole point of measuring rather than counting.

A per-tick PI feedback loop (`RATE_KP = 0.1`, capped integral) adjusts the next tick's byte budget
towards the target, so a workload converges instead of reacting to one noisy sample. `converged` is
reported per direction only against targets that were actually requested: a zero target means that
knob was not asked for, and comparing a measured zero with zero would be vacuous.

The actual write rate counts what reached the page cache, so it can be honest about `pwrite`
without being honest about the platter. `loadlab_io_fsyncs_per_second` and
`loadlab_io_fsyncs_total` are the counterweight: a write target with no `fsync` is a page-cache
workload, and one with `fsync` is a durability workload, and the two should not be read as the same
number.

### Operation cap

`io.iops` caps total read plus write operations per second. When `read_bps` and `write_bps` are both
zero and `iops` is non-zero, the operation budget is split evenly between reads and writes — the one
definition that needs no other knob to be unambiguous. When a byte target is present, `iops` is a
ceiling on top of it and the read/write ratio is preserved while the excess is dropped. Once the cap
binds the controller reports `saturated`, the same way an above-capacity CPU target does. A hard
safety bound (`MAX_OPS_PER_TICK`, one million) also reports `saturated` rather than being silently
exceeded.

### Block size, queue depth and pattern

`io.block_size` is both the size of every operation and the granularity of the region, so the region
is rounded **up** to a whole block. It may therefore exceed `--max-io-bytes` by less than one block;
the reported `loadlab_io_region_bytes` is the rounded value and is authoritative. `block_size` is
bounded to `1..=16MiB` and `queue_depth` to `1..=64`.

`io.pattern` is the same idea as the page-access patterns: `sequential` walks the region from the
start, `stride` steps by `io.stride_blocks` (default `1`) so a walk can skip blocks, and `random`
uses `io.random_seed` (default `1`) so a run is replayable. Offsets are block indices, so a walk
never straddles a block boundary.

`io.queue_depth` is the number of operations kept in flight per tick, executed by scoped worker
threads and joined before the tick returns. `1` is strictly serial. There is no persistent thread
pool: a tick with nothing to do starts no threads at all.

### Direct I/O

By default the I/O goes through the page cache, which is the right model for a workload meant to
measure what a reader or a filesystem cache absorbs. `io.direct: true` opens the file with
`O_DIRECT` instead, so each `pread`/`pwrite` is issued against the block device rather than against
cached pages, and the reported throughput and latency describe the device.

`direct` is reported back in both surfaces — `controller.io.direct` and `loadlab_io_direct` — so a
client can tell that it got what it asked for rather than having to infer it from a lower number.

Three things about it are deliberate.

**An unsupported filesystem is an error, not a fallback.** If the `open` with `O_DIRECT` fails, the
controller generates nothing for that tick and `controller.last_error` carries the reason, naming
both the flag and the alignment it needed. The alternative — issuing buffered I/O and reporting it
under a `direct: true` label — would hand back page-cache numbers to an operator who had explicitly
asked not to be given them. Unlike `CONFIG_SCHEDSTATS`, which is an extra observation layered on
top of a workload that runs either way, `O_DIRECT` *is* the requested workload.

**`block_size` must be a multiple of 4096.** A direct read or write has to be aligned in buffer
address, file offset and length to the filesystem's logical block size, and 4KiB is the one
filesystem block size across the Linux filesystems loadlab targets. A misaligned combination is
rejected on the way in with a 400 naming `io.block_size`, rather than accepted and left to fail
per-operation with `EINVAL`. The default is already 4096, so this only fires on a configuration that
deliberately asked for something else. Offsets and lengths are multiples of `block_size` and buffers
are allocated 4KiB-aligned, so the constraint holds without a runtime check.

**A direct read of a region that was never written does not reach the device.** The file starts
sparse, and ext4 satisfies a direct read of an unwritten extent by zero-filling it, so the syscall
returns without touching the block layer. Measured on this host, a direct read workload against a
fresh file showed a mean of **0.46µs**, while the same process reading a region it had already
written showed **12.90µs** — a factor of 28, and the difference between measuring zero-fill and
measuring a disk.

So a `direct` read-only workload started against a fresh loadlab reports the speed of `memset`.
Anything else is unaffected: a direct *write* allocates the extent and does reach the device (mean
16.32µs, max 13.18ms), and a direct read of already-written data behaves the same way.

This is not fixed here, because the honest fix is a policy question rather than a bug: the region
could be written before the first read, which costs the operator a startup delay and real writes to
their disk. Until that exists, **write before you measure a direct read**, or interpret a direct
read of a fresh file as measuring the filesystem's extent allocation path rather than its read path.

**Alignment is a portability guarantee, not a runtime-verified one.** loadlab always satisfies it,
but it does not prove the running kernel enforces it, because that is not a stable property: some
kernels and filesystems return `EINVAL`, while ext4 on a current kernel accepts a misaligned direct
read. So there is no assertion that a misaligned read fails — such a test would be asserting one
host's leniency and would then fail, correctly, on a stricter one.

### Latency

Throughput answers how much moved; latency is what tells you whether a device is keeping up. Three
operations are timed separately, because averaging them would describe none of them: on most devices
an `fsync` is orders of magnitude slower than the `pwrite` that precedes it.

`read_latency`, `write_latency` and `fsync_latency` each report:

| field | meaning |
| --- | --- |
| `count` | operations observed, cumulative since the controller started |
| `sum_seconds` | total time in those operations |
| `min_seconds` | fastest observation, or `null` if there has been none |
| `max_seconds` | slowest observation, or `null` if there has been none |

`null` rather than `0` for an unobserved minimum or maximum: zero would be a claim that the
operation took no time at all.

**No percentiles are published.** A percentile over a histogram is an interpolation within a bucket,
and an interpolated number wearing no label is the failure mode this project exists to avoid. Use
`histogram_quantile` over the Prometheus families `loadlab_io_read_latency_seconds`,
`loadlab_io_write_latency_seconds` and `loadlab_io_fsync_latency_seconds`, and know that the result
is bucket-interpolated. Bucket boundaries are 100µs, 250µs, 500µs, 1ms, 2.5ms, 5ms, 10ms, 50ms,
500ms and 1s, a fixed set that does not grow with traffic.

The latencies are cumulative and never reset, so averaging `sum_seconds` between two scrapes and
dividing by the change in `count` gives the mean over that interval. Timing costs two `Instant`
reads per operation, which is under a tenth of a percent of even a page-cache hit; what it does
perturb is the measurement of very fast operations, which is one more reason the result is a
histogram rather than a mean.

With `queue_depth` above `1` the workers overlap, so these are per-operation *service* times, not
queueing delay. The queueing delay shows up in the gap between the requested rate and the actual one.

### Disk ceiling

```bash
cargo run --release -- --max-io-bytes 268435456   # or LOADLAB_MAX_IO_BYTES
```

Defaults to 1 GiB and is separate from `--max-file-backed-bytes` and `--max-memory-bytes` because
the I/O region is a real file and costs disk, not RAM. It bounds the region the controller walks;
unlike the file-backed disk ceiling it is not an error or a saturation condition when a target
implies a larger region, because the walk simply wraps inside the region. The effective ceiling is
exported as `loadlab_configured_max_io_bytes` so a rule can use the operator's own number.

### What the controller reports

Under `controller.io` in `GET /metrics`:

| field | meaning |
| --- | --- |
| `path` | the file loadlab created, or `""` until a rate is requested |
| `region_bytes` | block-aligned size of the region being walked |
| `block_size_bytes` | block size used for every operation |
| `direct` | whether the file was opened with `O_DIRECT` |
| `queue_depth` | maximum in-flight operations per tick |
| `pattern` | `"sequential"`, `"random"` or `"stride"` |
| `read_bps` / `write_bps` | requested throughput, after `running` is applied |
| `read_bps_actual` / `write_bps_actual` | measured throughput from bytes actually moved |
| `iops` / `iops_actual` | requested cap and measured operation rate |
| `fsync_rate` / `fsyncs_per_second` | requested and measured `fsync` rate |
| `read_bytes_total` / `write_bytes_total` | cumulative bytes moved |
| `ops_total` / `fsyncs_total` | cumulative operations and `fsync` calls |
| `read_latency` / `write_latency` / `fsync_latency` | `count`, `sum_seconds`, `min_seconds`, `max_seconds` per operation kind |
| `converged` | every requested rate is within tolerance of what was measured |
| `saturated` | an operation cap or the safety bound stopped the byte target |

