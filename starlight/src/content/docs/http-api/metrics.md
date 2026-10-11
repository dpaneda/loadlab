---
title: "Metrics and Prometheus"
---

## GET /metrics

Returns process observations and controller state as JSON.

The schema is deliberately JSON-first: it is the endpoint to read when you want to see everything at
once, including the `last_error` string. For time series, scrape [`GET /prometheus`](#get-prometheus)
instead, which exposes the same numbers in the text format.

Notable fields:

| Field | Meaning |
| --- | --- |
| `process.memory.peak_rss_bytes` | `VmHWM`: high-water mark of RSS since start, so a peak between two scrapes is not lost |
| `process.cpu.user_seconds` / `system_seconds` | CPU time, normalised from clock ticks |
| `process.cpu.threads` | thread count |
| `controller.last_target_arena_bytes` | what the controller last aimed for |
| `controller.convergence_error_bytes` | target minus actual, signed |
| `controller.converged` | whether the last tick landed within tolerance |
| `controller.reconcile_errors_total` | cumulative failures, because `last_error` is cleared on the next good tick |
| `controller.cpu.*` | the CPU controller's own view, documented under [CPU utilization](../../controllers/cpu/#cpu-utilization) |
| `controller.memory_access.*` | the anonymous-arena fault loop; see [Page access patterns](../../controllers/page-access/#page-access-patterns) |
| `controller.file_backed.*` | the file-backed controller; see [File-backed memory](../../controllers/file-backed/#file-backed-memory) |
| `controller.shared.*` | the shared-memory and copy-on-write controller; see [Shared memory and copy-on-write](../../controllers/shared/#shared-memory-and-copy-on-write) |
| `controller.io.*` | the disk I/O controller; see [Disk I/O](../../controllers/io/#disk-io) |
| `controller.network.*` | the TCP/UDP traffic generator; see [Network](../../controllers/network/#network) |
| `experiment.*` | which run these numbers belong to; see [The experiment](../../experiment/#the-experiment) |
| `host.*` | the host the numbers were produced on; see [The host](../../host/#the-host) |
| `controller.thp.*` | the requested/effective transparent huge page policy; see [Transparent huge pages](../../controllers/thp/#transparent-huge-pages) |

There is no `scenarios.active` field here. The active scenario is published under
`experiment.active_scenario`, because it is part of the run's identity rather than a property of the
controller; [`GET /scenarios`](../scenarios/#get-scenarios) is where the scenario store itself is listed.

## GET /prometheus

Returns the same observations in the Prometheus text exposition format, version 0.0.4, as
`text/plain`.

This is the scrape target. It sits beside `/metrics` rather than replacing it, because the JSON
endpoint is what the docs and tooling read and it carries a human-readable error string that has no
good metric form.

```bash
curl -sS http://127.0.0.1:8080/prometheus
```

### Labels and cardinality

The exposition carries four labelled metrics and no others:

| Metric | Label | Values |
| --- | --- | --- |
| `loadlab_build_info` | `version` | the compile-time crate version |
| `loadlab_host_info` | `kernel_name`, `kernel_release`, `machine`, `cpu_model`, `cpus` | this machine; one series, always `1` |
| `loadlab_thp_requested` | `policy` | `default`, `always`, `madvise`, `never` |
| `loadlab_thp_host` | `policy` | `default`, `always`, `madvise`, `never` |

Every other metric is a bare gauge or counter, so the series count is fixed and known without
scraping. Nothing a client controls becomes a label value: not a workload expression, not a network
target, not a file path, not a scenario name or version. Those are unbounded and operator-supplied,
so any of them as a label would make the cardinality of the exposition a function of its own input,
which is how a metrics backend falls over. The four policies are a closed enumeration published in
full on every scrape, so the THP series do not appear and disappear with the requested setting.

`loadlab_host_info` is the one place where label values come from outside the process, and it is
there because a measurement without it cannot be interpreted: the same series on a 4-core host and
on a 64-core one are the same numbers meaning different things. Its values are bounded by the
machine rather than chosen by a caller, so it contributes exactly one series per process and cannot
grow. `cpu_model` is `unknown` when `/proc/cpuinfo` does not carry one, because an empty label
matches nothing in a query written for a real model.

The experiment seed is deliberately **not** a label, for the reason above taken one step further: it
is client-supplied *and* it changes between runs, so as a label it would mint a new series per seed
and fragment the history behind each one. It is a gauge value instead, so `changes()` finds the runs
that used different seeds.

### Metric names

Names are stable: `loadlab_` prefix, `_total` suffix on counters, and `_total` on nothing else.
Byte values are bytes, not MiB -- scale in the query, so the graph can be read in whatever unit
makes sense. The exposition is built by the
[`prometheus`](https://crates.io/crates/prometheus) crate (pure Rust, `default-features = false`),
so metric types, label escaping and text encoding are handled by a tested library rather than by
string formatting here.

| Metric | Type | Meaning |
| --- | --- | --- |
| `loadlab_build_info` | gauge | always 1, labelled with `version` |
| `loadlab_host_info` | gauge | always 1, labelled with the kernel, machine, CPU model and core count |
| `loadlab_process_uptime_seconds` | gauge | seconds since process start |
| `loadlab_experiment_seed` | gauge | the configured seed, or `0` when none is |
| `loadlab_experiment_elapsed_seconds` | gauge | seconds since `t = 0` for the current run |
| `loadlab_workload_running` | gauge | 1 when `running` is true |
| `loadlab_configured_max_memory_bytes` | gauge | effective operator ceiling |
| `loadlab_configured_tolerance_bytes` | gauge | convergence tolerance |
| `loadlab_configured_tick_ms` | gauge | controller tick interval, so the staleness rule is writable in PromQL |
| `loadlab_configured_cpu_workers` | gauge | worker threads the configuration asks for; `0` means one per logical CPU |
| `loadlab_configured_cpu_tolerance_percent` | gauge | CPU convergence tolerance, in points of a core |
| `loadlab_cpu_measurement_resolution_percent` | gauge | finest CPU difference one measurement can express, `1000 / tick_ms` |
| `loadlab_controller_target_arena_bytes` | gauge | what the controller last aimed for |
| `loadlab_controller_arena_resident_bytes` | gauge | what the arena holds |
| `loadlab_controller_convergence_error_bytes` | gauge | target minus actual, signed; positive is below target |
| `loadlab_controller_converged` | gauge | 1 when the last tick was within tolerance |
| `loadlab_controller_ticks_total` | counter | ticks completed |
| `loadlab_controller_reconcile_errors_total` | counter | ticks that failed |
| `loadlab_controller_error` | gauge | 1 when the most recent tick failed |
| `loadlab_controller_last_tick_age_seconds` | gauge | staleness of the controller; `NaN` before the first tick |
| `loadlab_memory_rss_bytes` | gauge | total RSS |
| `loadlab_memory_peak_rss_bytes` | gauge | RSS high-water mark since start |
| `loadlab_memory_rss_anon_bytes` | gauge | anonymous RSS, the quantity the workload controls |
| `loadlab_memory_baseline_rss_anon_bytes` | gauge | anonymous RSS at startup, so the workload's own contribution is visible as a delta |
| `loadlab_memory_rss_file_bytes` | gauge | file-backed RSS, which loadlab does not control |
| `loadlab_memory_rss_shmem_bytes` | gauge | shared resident memory |
| `loadlab_memory_pss_bytes` | gauge | proportional set size |
| `loadlab_memory_swap_bytes` | gauge | expected to stay at 0 |
| `loadlab_memory_anon_huge_pages_bytes` | gauge | anonymous huge pages |
| `loadlab_memory_anonymous_bytes` | gauge | anonymous mapped, resident or not |
| `loadlab_memory_private_dirty_bytes` | gauge | private dirty pages |
| `loadlab_thp_requested` | gauge | one series per policy (`default`/`always`/`madvise`/`never`); `1` on the requested one |
| `loadlab_thp_host` | gauge | one series per policy; `1` on the host's setting |
| `loadlab_thp_host_enabled` | gauge | `1` when the host has a readable THP knob, `0` otherwise |
| `loadlab_faults_minor_total` | counter | minor page faults |
| `loadlab_faults_major_total` | counter | major page faults |
| `loadlab_faults_generated_minor_total` | counter | minor faults taken by the controller thread |
| `loadlab_faults_generated_major_total` | counter | major faults taken by the controller thread |
| `loadlab_faults_incidental_minor_total` | counter | minor faults taken by every other thread |
| `loadlab_faults_incidental_major_total` | counter | major faults taken by every other thread |
| `loadlab_process_cpu_user_seconds_total` | counter | user CPU seconds |
| `loadlab_process_cpu_system_seconds_total` | counter | system CPU seconds |
| `loadlab_process_threads` | gauge | thread count |
| `loadlab_cpu_workers` | gauge | CPU worker threads running now; `0` whenever the target is zero |
| `loadlab_cpu_worker_scheduling_available` | gauge | `1` when the kernel exposes per-thread scheduler counters; the run-queue series below are meaningless without it |
| `loadlab_cpu_worker_runqueue_wait_nanoseconds` | gauge | time the pool spent runnable but not running; resets with the pool, see below |
| `loadlab_cpu_worker_worst_runqueue_wait_nanoseconds` | gauge | the largest single-worker run-queue wait |
| `loadlab_cpu_capacity_percent` | gauge | `workers * 100` |
| `loadlab_cpu_requested_utilization_percent` | gauge | what the expression asked for, before clamping |
| `loadlab_cpu_target_utilization_percent` | gauge | what the pool was told to deliver, after clamping |
| `loadlab_cpu_actual_utilization_percent` | gauge | smoothed measured CPU, same unit as the target |
| `loadlab_cpu_duty_cycle` | gauge | fraction of a core each worker is burning, `0.0..=1.0` |
| `loadlab_cpu_converged` | gauge | 1 when the last tick was within tolerance and not saturated |
| `loadlab_cpu_saturated` | gauge | 1 when the request exceeded capacity |
| `loadlab_memory_access_target_pages_per_second` | gauge | requested fault rate against the anonymous arena |
| `loadlab_memory_access_actual_pages_per_second` | gauge | measured fault rate; process-wide, so an upper bound |
| `loadlab_memory_access_minor_faults_rate` | gauge | measured minor fault rate, faults/second |
| `loadlab_memory_access_major_faults_rate` | gauge | measured major fault rate, faults/second |
| `loadlab_memory_access_converged` | gauge | 1 when the fault loop is at its requested rate |
| `loadlab_memory_access_saturated` | gauge | 1 when the requested rate exceeds what one pass can produce |
| `loadlab_configured_max_file_backed_bytes` | gauge | disk ceiling the operator set on the file-backed file |
| `loadlab_file_backed_rss_bytes` | gauge | resident bytes in loadlab's file-backed mapping |
| `loadlab_file_backed_pages` | gauge | resident pages in that mapping |
| `loadlab_file_backed_file_bytes` | gauge | current length of the file |
| `loadlab_file_backed_requested_rss_bytes` | gauge | evaluated target, before the disk ceiling |
| `loadlab_file_backed_target_rss_bytes` | gauge | target after clamping to the disk ceiling |
| `loadlab_file_backed_pages_per_second` | gauge | requested fault rate against the file |
| `loadlab_file_backed_actual_pages_per_second` | gauge | measured fault rate; process-wide, so an upper bound |
| `loadlab_file_backed_minor_faults_rate` | gauge | kernel minor fault rate, process-wide |
| `loadlab_file_backed_major_faults_rate` | gauge | kernel major fault rate, process-wide |
| `loadlab_file_backed_converged` | gauge | 1 when both the residency and the rate are on target |
| `loadlab_file_backed_saturated` | gauge | 1 when the request exceeded the disk ceiling or the rate was unreachable |
| `loadlab_configured_max_shared_bytes` | gauge | combined ceiling on the shared segment plus copy-on-write |
| `loadlab_shared_segment_bytes` | gauge | size of the shared segment |
| `loadlab_shared_private_bytes_per_actor` | gauge | effective per-actor private view |
| `loadlab_shared_actors` | gauge | live actors, including loadlab itself |
| `loadlab_shared_child_actors` | gauge | live forked actors |
| `loadlab_shared_segment_physical_bytes` | gauge | the shared segment counted once |
| `loadlab_shared_segment_rss_bytes` | gauge | `sum(RssShmem)`: the segment counted once per mapper |
| `loadlab_shared_overcount_bytes` | gauge | the double-count, `segment_rss - segment_physical` |
| `loadlab_shared_cow_physical_bytes` | gauge | physical copy-on-write pages |
| `loadlab_shared_rss_bytes` | gauge | `sum(Rss)` over every actor |
| `loadlab_shared_pss_bytes` | gauge | `sum(Pss)` over every actor |
| `loadlab_shared_requested_cow_bytes` | gauge | copy-on-write request before the combined ceiling |
| `loadlab_shared_target_cow_bytes` | gauge | copy-on-write target after clamping |
| `loadlab_shared_converged` | gauge | 1 when copy-on-write is at target |
| `loadlab_shared_saturated` | gauge | 1 when the request exceeded the combined ceiling |
| `loadlab_configured_max_io_bytes` | gauge | ceiling the operator set on the I/O scratch file |
| `loadlab_io_region_bytes` | gauge | block-aligned size of the region the I/O controller walks |
| `loadlab_io_block_size_bytes` | gauge | block size used for every read and write |
| `loadlab_io_queue_depth` | gauge | maximum operations kept in flight per tick |
| `loadlab_io_read_bps` | gauge | requested read throughput, bytes per second |
| `loadlab_io_read_bps_actual` | gauge | measured read throughput from bytes actually returned |
| `loadlab_io_write_bps` | gauge | requested write throughput, bytes per second |
| `loadlab_io_write_bps_actual` | gauge | measured write throughput from bytes actually stored |
| `loadlab_io_iops` | gauge | requested operation cap, total reads plus writes per second |
| `loadlab_io_iops_actual` | gauge | measured total operations per second |
| `loadlab_io_fsync_rate` | gauge | requested `fsync` calls per second |
| `loadlab_io_fsyncs_per_second` | gauge | measured `fsync` calls per second |
| `loadlab_io_read_bytes_total` | counter | cumulative bytes returned by reads |
| `loadlab_io_write_bytes_total` | counter | cumulative bytes stored by writes |
| `loadlab_io_ops_total` | counter | cumulative read plus write operations |
| `loadlab_io_fsyncs_total` | counter | cumulative `fsync` calls |
| `loadlab_io_converged` | gauge | 1 when every requested I/O rate is at target |
| `loadlab_io_saturated` | gauge | 1 when an operation cap or the safety bound bound the byte target |
| `loadlab_configured_max_network_ops_per_second` | gauge | operations-per-second ceiling the operator set on network traffic |
| `loadlab_network_concurrency` | gauge | worker slots and concurrent TCP connections or UDP sockets |
| `loadlab_network_message_size_bytes` | gauge | bytes in each message |
| `loadlab_network_connections_per_second` | gauge | requested fresh TCP connections per second |
| `loadlab_network_connections_per_second_actual` | gauge | measured fresh TCP connections per second |
| `loadlab_network_messages_per_second` | gauge | requested messages per second |
| `loadlab_network_messages_per_second_actual` | gauge | measured messages per second |
| `loadlab_network_bytes_per_second_actual` | gauge | measured total bytes per second |
| `loadlab_network_messages_total` | counter | cumulative messages sent over persistent connections or UDP |
| `loadlab_network_bytes_total` | counter | cumulative bytes accepted by sockets |
| `loadlab_network_connections_total` | counter | cumulative fresh TCP connections established |
| `loadlab_network_errors_total` | counter | cumulative socket errors (connect, write, send) |
| `loadlab_network_converged` | gauge | 1 when every requested network rate is at target |
| `loadlab_network_saturated` | gauge | 1 when the operator's ops/s ceiling stopped the request |

### Counter semantics

The counters are derived from absolute readings of `/proc/self/stat` and the runtime, which are
differenced per scrape and accumulated into a Prometheus `Counter`. A counter therefore starts at
zero when the process starts: the first scrape after a restart publishes the total accumulated
since then, and every later scrape adds only what changed. A reading that goes backwards (which
`/proc` counters never do, but a `NaN` read or a logic error might cause) is ignored rather than
subtracted, so a counter never decreases. Use `rate()` and `increase()` as usual; both are
unaffected by the first-scrape baseline.

### Alerting notes

- `loadlab_cpu_worker_runqueue_wait_nanoseconds` climbing while `loadlab_cpu_actual_utilization_percent`
  sits below its target means the host scheduler is the limit, not the duty cycle: the controller is
  asking for work the machine will not schedule. Adding workers will not help, and lowering
  `cpu.workers` until the pool fits will. Gate the panel on
  `loadlab_cpu_worker_scheduling_available`, or a kernel without `CONFIG_SCHEDSTATS` reads as a
  permanently healthy host.
- `loadlab_controller_last_tick_age_seconds` is the liveness signal. The controller runs on its own thread, so
  a wedged or dead controller does not stop the HTTP server from answering: a scrape will keep
  succeeding while the workload quietly stops tracking. Alert on
  `loadlab_controller_last_tick_age_seconds > 5 * loadlab_configured_tick_ms / 1000`, not on scrape
  success. The `5 *` factor is what makes the rule survive a change of `tick_ms` at runtime; the
  ready-made rule is in `contrib/prometheus/loadlab.rules.yml`.
- `loadlab_controller_error` catches a *current* failure but is cleared by the next good tick.
  `loadlab_controller_reconcile_errors_total` catches intermittent ones, which is usually the more
  interesting signal.
- `loadlab_memory_peak_rss_bytes` is the only metric that cannot miss a peak. `loadlab_memory_rss_bytes` sampled
  at 15s over a workload with a 6s cycle will under-report the maximum by a wide margin.
- CPU saturation and CPU non-convergence are two names for one mistake and are worth alerting on
  separately. A pool asked for more than it can deliver reports `loadlab_cpu_saturated` *and*
  fails to converge, but the fix is the same — more workers — so alerting on both double-pages for
  a single misconfiguration. Alert on `loadlab_cpu_saturated` for "the request is above capacity",
  and on `loadlab_cpu_converged == 0 and loadlab_cpu_saturated == 0` for "the machine will not
  give back the CPU that was asked for".
- `loadlab_io_read_bps_actual` and `loadlab_io_write_bps_actual` are the honest numbers, and the
  requested gauges beside them are the target. A sustained gap between the two is the filesystem
  refusing the request, not the controller misbehaving; the ready-made rule
  `loadlab:io_throughput_shortfall_ratio` is in the alerting rules. A write rate with no
  `fsync` is a page-cache rate, so pair it with `loadlab_io_fsyncs_per_second` before concluding
  anything reached the block layer.
- Network saturation (`loadlab_network_saturated == 1`) and non-convergence (`loadlab_network_converged == 0`)
  should be monitored separately, just like CPU and I/O. Saturated means the requested operation rates
  exceeded `--max-network-ops-per-second`. Non-converged while running and not saturated indicates
  destination bottlenecks, connection dropouts, or network limits preventing the generator from
  achieving the desired rate.
- `loadlab_network_errors_total` tracks failed socket operations (connects, writes, sends). An increasing
  counter indicates unreachable targets, closed connections, or network dropouts. Alert on
  `increase(loadlab_network_errors_total[5m]) > 0`.
- `loadlab_experiment_elapsed_seconds` going *backwards* is a new run, not a fault. It resets when the
  workload starts and when `experiment.start_time` changes, so a recording that plots it as a
  continuous line will draw a cliff at every restart. Segment on it, or join it against
  `loadlab_experiment_seed` and `loadlab_host_info` and treat each `(seed, host)` pair as its own
  series. `loadlab_experiment_seed == 0` means no seed was configured, so that run is not
  reproducible and should not be compared against one that was.

### Scraping cost

Every scrape reads `/proc/self/smaps_rollup`, which walks the page tables and so costs more the
larger the arena is. At a 15s interval that is noise. At 1s against a 600 MiB arena it is real CPU,
and the counter to watch for it is `loadlab_process_cpu_user_seconds_total`.

