//! Prometheus exposition for the process and the controller.
//!
//! `/metrics` stays JSON because that is the machine-friendly state endpoint described in
//! `docs/src/http-api/metrics.md`; this module is the scrape target and lives beside it rather than
//! replacing it.
//!
//! The `prometheus` crate does the encoding. It is worth the dependency: it escapes HELP text and
//! label values, which hand-written output gets wrong the moment either contains a quote or a
//! backslash, and it refuses to register two metrics under one name. Neither is hypothetical --
//! `test::label_values_are_escaped` exists to keep that from regressing.
//!
//! The crate has no parser, so the format checks in the tests are still structural. The
//! authoritative parser is exercised out of band during verification.

use std::sync::Mutex;

use prometheus::{
    Counter, Encoder, Gauge, GaugeVec, Histogram, HistogramOpts, IntCounter, IntGauge, Opts,
    Registry, TextEncoder, opts,
};

use crate::{
    cpu::TOLERANCE_PCT,
    procfs::{self, CpuStats, FaultStats, HostInfo, MemoryStats},
    state::{RuntimeStatus, WorkloadState},
};

/// The finest difference in CPU utilization a measurement over `tick_ms` can express, in points
/// of a core.
///
/// `/proc/self/stat` reports CPU time in whole clock ticks, so a window of `tick_ms` contains
/// about `tick_ms * ticks_per_second / 1000` of them and the reading can only move in steps of
/// that size. At the default 100ms window and 100Hz tick that is 10 points of a core -- an
/// order of magnitude coarser than the 5-point convergence tolerance, which is why the two have
/// to be published side by side: a target below the resolution is served but not verifiable.
///
/// Derived rather than read from the controller so the exported value is exactly the
/// relationship an operator can reproduce by hand from `loadlab_configured_tick_ms`.
fn measurement_resolution_percent(tick_ms: u64) -> f64 {
    if tick_ms == 0 {
        return 0.0;
    }
    // SAFETY of the cast: a tick interval in milliseconds, nowhere near 2^53, and a lossy
    // conversion would change the resolution by a negligible fraction of a point.
    #[allow(clippy::cast_precision_loss)]
    let tick_ms = tick_ms as f64;
    100.0 / (tick_ms / 1000.0 / procfs::clock_tick_seconds())
}

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

pub struct Metrics {
    registry: Registry,
    build_info: IntGauge,
    process_uptime_seconds: Gauge,
    process_threads: IntGauge,
    host_info: IntGauge,
    experiment_seed: IntGauge,
    experiment_elapsed_seconds: Gauge,
    workload_running: IntGauge,
    configured_max_memory_bytes: IntGauge,
    configured_max_file_backed_bytes: IntGauge,
    configured_max_shared_bytes: IntGauge,
    configured_max_io_bytes: IntGauge,
    configured_tolerance_bytes: IntGauge,
    configured_tick_ms: IntGauge,
    controller_target_arena_bytes: IntGauge,
    controller_arena_resident_bytes: IntGauge,
    controller_convergence_error_bytes: Gauge,
    controller_converged: IntGauge,
    controller_error: IntGauge,
    controller_last_tick_age_seconds: Gauge,
    controller_ticks_total: IntCounter,
    controller_reconcile_errors_total: IntCounter,
    memory_baseline_rss_anon_bytes: IntGauge,
    memory_rss_bytes: IntGauge,
    memory_peak_rss_bytes: IntGauge,
    memory_rss_anon_bytes: IntGauge,
    memory_rss_file_bytes: IntGauge,
    memory_rss_shmem_bytes: IntGauge,
    memory_pss_bytes: IntGauge,
    memory_swap_bytes: IntGauge,
    memory_anon_huge_pages_bytes: IntGauge,
    memory_anonymous_bytes: IntGauge,
    memory_private_dirty_bytes: IntGauge,
    faults_minor_total: IntCounter,
    faults_major_total: IntCounter,
    faults_generated_minor_total: IntCounter,
    faults_generated_major_total: IntCounter,
    faults_incidental_minor_total: IntCounter,
    faults_incidental_major_total: IntCounter,
    cpu_user_seconds_total: Counter,
    cpu_system_seconds_total: Counter,
    configured_cpu_workers: IntGauge,
    configured_cpu_tolerance_percent: Gauge,
    cpu_measurement_resolution_percent: Gauge,
    cpu_workers: IntGauge,
    cpu_worker_scheduling_available: IntGauge,
    cpu_worker_runqueue_wait_nanoseconds: IntGauge,
    cpu_worker_worst_runqueue_wait_nanoseconds: IntGauge,
    cpu_capacity_percent: Gauge,
    cpu_requested_utilization_percent: Gauge,
    cpu_target_utilization_percent: Gauge,
    cpu_actual_utilization_percent: Gauge,
    cpu_duty_cycle: Gauge,
    cpu_converged: IntGauge,
    cpu_saturated: IntGauge,
    memory_access_target_pages_per_second: Gauge,
    memory_access_actual_pages_per_second: Gauge,
    memory_access_converged: IntGauge,
    memory_access_saturated: IntGauge,
    memory_access_minor_faults_rate: Gauge,
    memory_access_major_faults_rate: Gauge,
    file_backed_rss_bytes: IntGauge,
    file_backed_pages: IntGauge,
    file_backed_file_bytes: IntGauge,
    file_backed_target_rss_bytes: IntGauge,
    file_backed_requested_rss_bytes: IntGauge,
    file_backed_converged: IntGauge,
    file_backed_saturated: IntGauge,
    file_backed_pages_per_second: Gauge,
    file_backed_actual_pages_per_second: Gauge,
    file_backed_minor_faults_rate: Gauge,
    file_backed_major_faults_rate: Gauge,
    shared_segment_bytes: IntGauge,
    shared_private_bytes_per_actor: IntGauge,
    shared_actors: IntGauge,
    shared_child_actors: IntGauge,
    shared_segment_physical_bytes: IntGauge,
    shared_segment_rss_bytes: IntGauge,
    shared_overcount_bytes: IntGauge,
    shared_cow_physical_bytes: IntGauge,
    shared_rss_bytes: IntGauge,
    shared_pss_bytes: IntGauge,
    shared_requested_cow_bytes: IntGauge,
    shared_target_cow_bytes: IntGauge,
    shared_converged: IntGauge,
    shared_saturated: IntGauge,
    io_region_bytes: IntGauge,
    io_block_size_bytes: IntGauge,
    io_queue_depth: IntGauge,
    io_read_bps: Gauge,
    io_read_bps_actual: Gauge,
    io_write_bps: Gauge,
    io_write_bps_actual: Gauge,
    io_iops: Gauge,
    io_iops_actual: Gauge,
    io_fsync_rate: Gauge,
    io_fsyncs_per_second: Gauge,
    io_read_bytes_total: IntCounter,
    io_write_bytes_total: IntCounter,
    io_ops_total: IntCounter,
    io_fsyncs_total: IntCounter,
    /// Whether the file was opened with `O_DIRECT`. 1 or 0.
    io_direct: IntGauge,
    io_read_latency: Histogram,
    io_write_latency: Histogram,
    io_fsync_latency: Histogram,
    io_converged: IntGauge,
    io_saturated: IntGauge,
    configured_max_network_ops_per_second: IntGauge,
    network_concurrency: IntGauge,
    network_message_size_bytes: IntGauge,
    network_connections_per_second: Gauge,
    network_connections_per_second_actual: Gauge,
    network_messages_per_second: Gauge,
    network_messages_per_second_actual: Gauge,
    network_bytes_per_second_actual: Gauge,
    network_messages_total: IntCounter,
    network_bytes_total: IntCounter,
    network_connections_total: IntCounter,
    network_errors_total: IntCounter,
    network_converged: IntGauge,
    network_saturated: IntGauge,
    thp_requested: GaugeVec,
    thp_host: GaugeVec,
    thp_host_enabled: IntGauge,
    /// Last absolute readings published to the counters above.
    seen: Mutex<Seen>,
}

#[derive(Default)]
struct Seen {
    ticks: f64,
    reconcile_errors: f64,
    minor_faults: f64,
    major_faults: f64,
    generated_minor_faults: f64,
    generated_major_faults: f64,
    incidental_minor_faults: f64,
    incidental_major_faults: f64,
    cpu_user: f64,
    cpu_system: f64,
    io_read_bytes: f64,
    io_write_bytes: f64,
    io_ops: f64,
    io_fsyncs: f64,
    network_messages: f64,
    network_bytes: f64,
    network_connections: f64,
    network_errors: f64,
}

impl Metrics {
    /// Build the registry. Metric names are fixed here and validated for uniqueness at
    /// construction, so a duplicate is a startup failure rather than malformed scrape output.
    ///
    /// Long by nature: it is a flat list of metric definitions, and splitting it would only
    /// move the same lines somewhere less obvious.
    #[allow(clippy::too_many_lines)]
    pub fn new(host: &HostInfo) -> Result<Self, prometheus::Error> {
        let metrics = Self {
            registry: Registry::new(),
            build_info: gauge(
                Opts::new(
                    "loadlab_build_info",
                    "Build information for the running loadlab process. Always 1.",
                )
                .const_label("version", env!("CARGO_PKG_VERSION")),
            )?,
            process_uptime_seconds: float_gauge(
                "loadlab_process_uptime_seconds",
                "Seconds since the loadlab process started.",
            )?,
            // One series, always 1, with the host's identity as labels. The standard Prometheus
            // idiom for metadata, and the only place here where labels are derived from something
            // outside the workload: they are bounded by the machine, not by anything a client can
            // type, so this cannot grow.
            host_info: gauge(
                Opts::new(
                    "loadlab_host_info",
                    "The host this measurement describes, as labels. Always 1. Join other loadlab \
                     series to it to tell a run on a 4-core host from the same series on a 64-core \
                     one.",
                )
                .const_label("kernel_name", &host.kernel_name)
                .const_label("kernel_release", &host.kernel_release)
                .const_label("machine", &host.machine)
                .const_label("cpu_model", host.cpu_model.as_deref().unwrap_or("unknown"))
                .const_label("cpus", host.cpus.to_string().as_str()),
            )?,
            // The seed is a value rather than a label on purpose. It is client-supplied, so as a
            // label every distinct seed would mint a new series and the time series behind them
            // would fragment; as a gauge it is one series whose value changes, and
            // `changes()` finds the runs that used different seeds.
            experiment_seed: gauge(opts!(
                "loadlab_experiment_seed",
                "The experiment seed, or 0 when none is configured. Changes are what a reproducible \
                 run is keyed on: same seed, same workload state and same start time means the \
                 same walk."
            ))?,
            experiment_elapsed_seconds: float_gauge(
                "loadlab_experiment_elapsed_seconds",
                "Seconds since t=0 for the current run, the value every expression in the workload \
                 is evaluated against. Unlike loadlab_process_uptime_seconds it restarts when the \
                 workload does, and is offset by experiment.start_time when one is configured.",
            )?,
            process_threads: gauge(opts!(
                "loadlab_process_threads",
                "Threads in the loadlab process, which is the controller plus the async runtime."
            ))?,
            workload_running: gauge(opts!(
                "loadlab_workload_running",
                "Whether the workload is running. 1 or 0."
            ))?,
            configured_max_file_backed_bytes: gauge(opts!(
                "loadlab_configured_max_file_backed_bytes",
                "Ceiling the operator set on the file-backed file. Separate from \
                 loadlab_configured_max_memory_bytes because file-backed memory also costs disk."
            ))?,
            configured_max_shared_bytes: gauge(opts!(
                "loadlab_configured_max_shared_bytes",
                "Ceiling the operator set on the combined shared plus copy-on-write footprint. \
                 Separate from the other ceilings because the cost is shared_bytes once plus \
                 private_bytes * actors * cow_write_ratio, which no single requested field shows."
            ))?,
            configured_max_io_bytes: gauge(opts!(
                "loadlab_configured_max_io_bytes",
                "Ceiling the operator set on the temporary file the disk I/O controller creates. \
                 Separate from the memory ceilings because the I/O region is a real file and costs \
                 disk, not RAM."
            ))?,
            configured_max_memory_bytes: gauge(opts!(
                "loadlab_configured_max_memory_bytes",
                "Ceiling the operator set on generated anonymous memory, after clamping to the \
                 addressable limit of this platform."
            ))?,
            configured_tolerance_bytes: gauge(opts!(
                "loadlab_configured_tolerance_bytes",
                "Convergence tolerance the controller aims for."
            ))?,
            configured_tick_ms: gauge(opts!(
                "loadlab_configured_tick_ms",
                "Controller tick interval in milliseconds. Exported so the staleness alert can be \
                 written as a function of the configured tick rather than a hardcoded constant."
            ))?,
            controller_target_arena_bytes: gauge(opts!(
                "loadlab_controller_target_arena_bytes",
                "Bytes of generated anonymous memory the controller last aimed for."
            ))?,
            controller_arena_resident_bytes: gauge(opts!(
                "loadlab_controller_arena_resident_bytes",
                "Bytes of anonymous memory the arena currently holds."
            ))?,
            controller_convergence_error_bytes: float_gauge(
                "loadlab_controller_convergence_error_bytes",
                "Target minus actual, in bytes. Positive means the arena is below target.",
            )?,
            controller_converged: gauge(opts!(
                "loadlab_controller_converged",
                "Whether the last tick landed within tolerance of its target. 1 or 0."
            ))?,
            controller_error: gauge(opts!(
                "loadlab_controller_error",
                "Whether the most recent controller tick reported an error. 1 or 0."
            ))?,
            controller_last_tick_age_seconds: float_gauge(
                "loadlab_controller_last_tick_age_seconds",
                "Seconds since the controller's last tick. NaN before the first tick.",
            )?,
            controller_ticks_total: counter(opts!(
                "loadlab_controller_ticks_total",
                "Controller ticks completed since process start."
            ))?,
            controller_reconcile_errors_total: counter(opts!(
                "loadlab_controller_reconcile_errors_total",
                "Controller ticks that failed to reconcile, since process start. A workload that \
                 fails intermittently is not visible in the instantaneous error state, so alert on \
                 this counter instead."
            ))?,
            memory_baseline_rss_anon_bytes: gauge(opts!(
                "loadlab_memory_baseline_rss_anon_bytes",
                "Process anonymous RSS at startup, which the controller subtracts from every target."
            ))?,
            memory_rss_bytes: gauge(opts!(
                "loadlab_memory_rss_bytes",
                "Total resident set size from procfs."
            ))?,
            memory_peak_rss_bytes: gauge(opts!(
                "loadlab_memory_peak_rss_bytes",
                "High-water mark of resident set size since process start. Unlike rss this does not \
                 miss a peak that happened between two scrapes."
            ))?,
            memory_rss_anon_bytes: gauge(opts!(
                "loadlab_memory_rss_anon_bytes",
                "Resident anonymous memory, the quantity the workload controls."
            ))?,
            memory_rss_file_bytes: gauge(opts!(
                "loadlab_memory_rss_file_bytes",
                "Resident file-backed memory, which loadlab does not control."
            ))?,
            memory_rss_shmem_bytes: gauge(opts!(
                "loadlab_memory_rss_shmem_bytes",
                "Resident shared memory."
            ))?,
            memory_pss_bytes: gauge(opts!(
                "loadlab_memory_pss_bytes",
                "Proportional set size, which divides shared pages fairly between users."
            ))?,
            memory_swap_bytes: gauge(opts!(
                "loadlab_memory_swap_bytes",
                "Swapped anonymous memory. Expected to stay at zero: the arena releases with \
                 madvise rather than swapping."
            ))?,
            memory_anon_huge_pages_bytes: gauge(opts!(
                "loadlab_memory_anon_huge_pages_bytes",
                "Anonymous huge pages mapped by the process."
            ))?,
            memory_anonymous_bytes: gauge(opts!(
                "loadlab_memory_anonymous_bytes",
                "Anonymous memory mapped, resident or not, from smaps rollup."
            ))?,
            memory_private_dirty_bytes: gauge(opts!(
                "loadlab_memory_private_dirty_bytes",
                "Private dirty pages, which cannot be written back to disk."
            ))?,
            faults_minor_total: counter(opts!(
                "loadlab_faults_minor_total",
                "Minor page faults since process start. Rises steadily as the arena grows."
            ))?,
            faults_major_total: counter(opts!(
                "loadlab_faults_major_total",
                "Major page faults since process start, each of which had to hit the device."
            ))?,
            faults_generated_minor_total: counter(opts!(
                "loadlab_faults_generated_minor_total",
                "Minor page faults taken by the controller thread, which is the thread that touches \
                 the generated pages. The subset of loadlab_faults_minor_total that loadlab caused on \
                 purpose, so the difference is what the rest of the process faulted incidentally."
            ))?,
            faults_generated_major_total: counter(opts!(
                "loadlab_faults_generated_major_total",
                "Major page faults taken by the controller thread."
            ))?,
            faults_incidental_minor_total: counter(opts!(
                "loadlab_faults_incidental_minor_total",
                "Minor page faults taken by threads other than the controller: the async runtime, \
                 the allocator and the HTTP server. Non-zero while a workload is quiet is normal and \
                 expected."
            ))?,
            faults_incidental_major_total: counter(opts!(
                "loadlab_faults_incidental_major_total",
                "Major page faults taken by threads other than the controller."
            ))?,
            cpu_user_seconds_total: float_counter(opts!(
                "loadlab_process_cpu_user_seconds_total",
                "User CPU seconds consumed by the process."
            ))?,
            cpu_system_seconds_total: float_counter(opts!(
                "loadlab_process_cpu_system_seconds_total",
                "System CPU seconds consumed by the process."
            ))?,
            configured_cpu_workers: gauge(opts!(
                "loadlab_configured_cpu_workers",
                "Worker threads the configuration asks for. Zero means one per logical CPU, which \
                 is why it can be zero while workers are running."
            ))?,
            configured_cpu_tolerance_percent: float_gauge(
                "loadlab_configured_cpu_tolerance_percent",
                "How far the measured CPU may sit from its target and still count as converged, in \
                 points of a core. Exported for the same reason as the memory tolerance: the alert \
                 should follow the controller's own definition rather than restate it.",
            )?,
            cpu_measurement_resolution_percent: float_gauge(
                "loadlab_cpu_measurement_resolution_percent",
                "The finest difference in CPU utilization a single measurement can express, in \
                 points of a core. /proc counts CPU time in clock ticks, so a reading over a window \
                 moves in whole ticks and a target finer than this cannot be verified -- below it the \
                 controller holds its feedforward instead of chasing the rounding.",
            )?,
            cpu_workers: gauge(opts!(
                "loadlab_cpu_workers",
                "CPU worker threads currently running. Zero while the workload is stopped: the \
                 threads are torn down rather than left idling."
            ))?,
            cpu_worker_scheduling_available: gauge(opts!(
                "loadlab_cpu_worker_scheduling_available",
                "Whether the kernel exposes per-thread scheduler statistics (CONFIG_SCHEDSTATS). \
                 Zero means the runqueue wait series below are zero because they could not be read, \
                 not because no worker ever waited, so a dashboard must gate on this."
            ))?,
            cpu_worker_runqueue_wait_nanoseconds: gauge(opts!(
                "loadlab_cpu_worker_runqueue_wait_nanoseconds",
                "Nanoseconds the CPU worker threads spent runnable but not running. The missing \
                 input to loadlab_cpu_actual_utilization_percent: a worker that waited for a CPU \
                 consumed none, so an under-target reading with wait climbing here is the scheduler \
                 rather than the duty cycle. A gauge, not a counter, because the pool is rebuilt \
                 when cpu.workers or cpu.cpus changes and this falls back to zero when it is; read a \
                 drop as the pool restarting. Use deriv() for nanoseconds per second."
            ))?,
            cpu_worker_worst_runqueue_wait_nanoseconds: gauge(opts!(
                "loadlab_cpu_worker_worst_runqueue_wait_nanoseconds",
                "The largest cumulative runqueue wait of any single worker thread. The sum says how \
                 much waiting happened; this says whether one worker was starved while the others \
                 ran."
            ))?,
            cpu_capacity_percent: float_gauge(
                "loadlab_cpu_capacity_percent",
                "Highest percent the pool can deliver with every worker saturated, which is \
                 workers times 100. A target above this is clamped and loadlab_cpu_saturated \
                 goes to 1.",
            )?,
            cpu_requested_utilization_percent: float_gauge(
                "loadlab_cpu_requested_utilization_percent",
                "What cpu.utilization asked for this tick, before clamping to capacity. 100 is \
                 one saturated logical CPU, 200 is two, and 100 times the machine's core count \
                 is all of it.",
            )?,
            cpu_target_utilization_percent: float_gauge(
                "loadlab_cpu_target_utilization_percent",
                "What the pool was actually told to deliver, after clamping to capacity. \
                 Compare against actual to see whether the controller is keeping up.",
            )?,
            cpu_actual_utilization_percent: float_gauge(
                "loadlab_cpu_actual_utilization_percent",
                "Smoothed CPU the process was actually charged over the last few ticks, in the \
                 same unit as cpu.utilization: 100 is one saturated logical CPU. This is the \
                 number to trust over any duty cycle, because it is what the kernel charged \
                 rather than what was asked for. Divide by the machine's core count for a \
                 fraction-of-the-box figure.",
            )?,
            cpu_duty_cycle: float_gauge(
                "loadlab_cpu_duty_cycle",
                "Fraction of one worker's core the workers were last told to keep busy, 0 to 1. \
                 A duty cycle is an input to the controller, not a measurement of the load.",
            )?,
            cpu_converged: gauge(opts!(
                "loadlab_cpu_converged",
                "Whether actual utilization is within tolerance of the target and the target was \
                 reachable. 1 or 0."
            ))?,
            cpu_saturated: gauge(opts!(
                "loadlab_cpu_saturated",
                "Whether the requested target was above what the pool can deliver. 1 means the \
                 request cannot be met on this host as configured, however long it is left \
                 running."
            ))?,
            memory_access_target_pages_per_second: float_gauge(
                "loadlab_memory_access_target_pages_per_second",
                "Target minor-fault rate the access controller aims for (pages per second).",
            )?,
            memory_access_actual_pages_per_second: float_gauge(
                "loadlab_memory_access_actual_pages_per_second",
                "Measured minor-fault rate from the access controller (pages per second).",
            )?,
            memory_access_converged: gauge(opts!(
                "loadlab_memory_access_converged",
                "Whether the measured fault rate is within tolerance of the target. 1 or 0."
            ))?,
            memory_access_saturated: gauge(opts!(
                "loadlab_memory_access_saturated",
                "Whether the target fault rate exceeds what the access buffer can deliver. 1 or 0."
            ))?,
            memory_access_minor_faults_rate: float_gauge(
                "loadlab_memory_access_minor_faults_rate",
                "Measured minor-fault rate while the anonymous access controller was running \
                 (faults per second). Process-wide: arena growth and every other thread \
                 contribute to it, so it is an upper bound on what the access loop produced.",
            )?,
            memory_access_major_faults_rate: float_gauge(
                "loadlab_memory_access_major_faults_rate",
                "Measured major-fault rate while the anonymous access controller was running \
                 (faults per second). An anonymous mapping should produce none.",
            )?,
            file_backed_rss_bytes: gauge(opts!(
                "loadlab_file_backed_rss_bytes",
                "Resident pages of loadlab's own file-backed mapping, in bytes. This is the \
                 quantity file_backed.rss controls, and it is not the same number as \
                 loadlab_memory_rss_file_bytes, which covers the whole process including the \
                 executable's text."
            ))?,
            file_backed_pages: gauge(opts!(
                "loadlab_file_backed_pages",
                "Resident pages of loadlab's own file-backed mapping. Tracked by loadlab rather \
                 than read back with mincore, which would walk the page table every scrape."
            ))?,
            file_backed_file_bytes: gauge(opts!(
                "loadlab_file_backed_file_bytes",
                "Length of the temporary file behind the file-backed mapping. Never falls while \
                 the process runs: the file is not truncated below a live mapping, because \
                 truncating one raises SIGBUS on the next access."
            ))?,
            file_backed_target_rss_bytes: gauge(opts!(
                "loadlab_file_backed_target_rss_bytes",
                "File-backed RSS the controller was actually held to this tick, after the \
                 operator's file ceiling."
            ))?,
            file_backed_requested_rss_bytes: gauge(opts!(
                "loadlab_file_backed_requested_rss_bytes",
                "File-backed RSS the expression asked for this tick, before clamping. Above \
                 loadlab_file_backed_target_rss_bytes means the request exceeded the ceiling."
            ))?,
            file_backed_converged: gauge(opts!(
                "loadlab_file_backed_converged",
                "Whether the file-backed mapping is within tolerance of its target and the fault \
                 rate is on target. 1 or 0."
            ))?,
            file_backed_saturated: gauge(opts!(
                "loadlab_file_backed_saturated",
                "Whether the file-backed request was above what the operator's file ceiling or the \
                 mapping's per-tick capacity allows. 1 or 0."
            ))?,
            file_backed_pages_per_second: float_gauge(
                "loadlab_file_backed_pages_per_second",
                "Target page-fault rate for the file-backed access loop (pages per second).",
            )?,
            file_backed_actual_pages_per_second: float_gauge(
                "loadlab_file_backed_actual_pages_per_second",
                "Measured page-fault rate for the file-backed access loop (pages per second). \
                 Process-wide and therefore an upper bound: the anonymous access controller and \
                 the arena contribute to the same counter.",
            )?,
            file_backed_minor_faults_rate: float_gauge(
                "loadlab_file_backed_minor_faults_rate",
                "Process-wide minor-fault rate while the file-backed controller was running \
                 (faults per second).",
            )?,
            file_backed_major_faults_rate: float_gauge(
                "loadlab_file_backed_major_faults_rate",
                "Process-wide major-fault rate while the file-backed controller was running \
                 (faults per second). The first touch of a file page is a major fault, so this \
                 rises with the file-backed working set even though nothing is reading a disk.",
            )?,
            shared_segment_bytes: gauge(opts!(
                "loadlab_shared_segment_bytes",
                "Size of the shared segment every actor maps MAP_SHARED. One copy of physical \
                 memory regardless of the actor count."
            ))?,
            shared_private_bytes_per_actor: gauge(opts!(
                "loadlab_shared_private_bytes_per_actor",
                "Size of the MAP_PRIVATE view each actor maps and writes, after clamping to the \
                 operator's shared ceiling. Not necessarily what was requested."
            ))?,
            shared_actors: gauge(opts!(
                "loadlab_shared_actors",
                "Live actors in the shared workload, including the loadlab process itself."
            ))?,
            shared_child_actors: gauge(opts!(
                "loadlab_shared_child_actors",
                "Live forked actor processes, excluding the loadlab process."
            ))?,
            shared_segment_physical_bytes: gauge(opts!(
                "loadlab_shared_segment_physical_bytes",
                "Physical memory the shared segment occupies, counted once."
            ))?,
            shared_segment_rss_bytes: gauge(opts!(
                "loadlab_shared_segment_rss_bytes",
                "The shared segment counted once per mapper: sum of RssShmem over every actor. \
                 Above loadlab_shared_segment_physical_bytes by the double-count."
            ))?,
            shared_overcount_bytes: gauge(opts!(
                "loadlab_shared_overcount_bytes",
                "Shared pages counted once per mapper rather than once in total: \
                 loadlab_shared_segment_rss_bytes minus loadlab_shared_segment_physical_bytes. \
                 Zero with a single actor."
            ))?,
            shared_cow_physical_bytes: gauge(opts!(
                "loadlab_shared_cow_physical_bytes",
                "Physical memory the actors' copy-on-write pages occupy, summed across actors."
            ))?,
            shared_rss_bytes: gauge(opts!(
                "loadlab_shared_rss_bytes",
                "sum(Rss) over every actor: what naive process accounting reports for the set."
            ))?,
            shared_pss_bytes: gauge(opts!(
                "loadlab_shared_pss_bytes",
                "sum(Pss) over every actor: what the set physically costs. Compare with \
                 loadlab_shared_rss_bytes; the gap between them is the duplication."
            ))?,
            shared_requested_cow_bytes: gauge(opts!(
                "loadlab_shared_requested_cow_bytes",
                "Copy-on-write bytes the expressions asked for this tick, before the shared \
                 ceiling. Above loadlab_shared_target_cow_bytes means the request was clamped."
            ))?,
            shared_target_cow_bytes: gauge(opts!(
                "loadlab_shared_target_cow_bytes",
                "Copy-on-write bytes the controller was held to this tick, after the shared ceiling."
            ))?,
            shared_converged: gauge(opts!(
                "loadlab_shared_converged",
                "Whether the actors have taken at least the copy-on-write pages asked for, within \
                 tolerance. 1 or 0. Copy-on-write cannot be undone, so overshoot is converged."
            ))?,
            shared_saturated: gauge(opts!(
                "loadlab_shared_saturated",
                "Whether the copy-on-write request was above what the shared ceiling allows. 1 or 0."
            ))?,
            io_region_bytes: gauge(opts!(
                "loadlab_io_region_bytes",
                "Block-aligned size of the file region the I/O controller walks."
            ))?,
            io_block_size_bytes: gauge(opts!(
                "loadlab_io_block_size_bytes",
                "Block size used for every read and write."
            ))?,
            io_queue_depth: gauge(opts!(
                "loadlab_io_queue_depth",
                "Maximum operations the I/O controller keeps in flight per tick."
            ))?,
            io_read_bps: float_gauge(
                "loadlab_io_read_bps",
                "Read throughput requested this tick, in bytes per second.",
            )?,
            io_read_bps_actual: float_gauge(
                "loadlab_io_read_bps_actual",
                "Read throughput measured from the bytes the reads actually returned, in bytes per \
                 second. Compare with loadlab_io_read_bps to see the filesystem's real limit.",
            )?,
            io_write_bps: float_gauge(
                "loadlab_io_write_bps",
                "Write throughput requested this tick, in bytes per second.",
            )?,
            io_write_bps_actual: float_gauge(
                "loadlab_io_write_bps_actual",
                "Write throughput measured from the bytes the writes actually stored, in bytes per \
                 second. Page-cache hits make this honest about what reached the block layer only \
                 when paired with loadlab_io_fsyncs_per_second.",
            )?,
            io_iops: float_gauge(
                "loadlab_io_iops",
                "Cap on total read plus write operations per second requested this tick. Zero means \
                 uncapped.",
            )?,
            io_iops_actual: float_gauge(
                "loadlab_io_iops_actual",
                "Measured read plus write operations per second.",
            )?,
            io_fsync_rate: float_gauge(
                "loadlab_io_fsync_rate",
                "Number of fsync calls per second requested this tick. Zero means none.",
            )?,
            io_fsyncs_per_second: float_gauge(
                "loadlab_io_fsyncs_per_second",
                "Measured fsync calls per second.",
            )?,
            io_read_bytes_total: counter(opts!(
                "loadlab_io_read_bytes_total",
                "Cumulative bytes returned by reads since the I/O controller was created."
            ))?,
            io_write_bytes_total: counter(opts!(
                "loadlab_io_write_bytes_total",
                "Cumulative bytes stored by writes since the I/O controller was created."
            ))?,
            io_ops_total: counter(opts!(
                "loadlab_io_ops_total",
                "Cumulative read plus write operations since the I/O controller was created."
            ))?,
            io_fsyncs_total: counter(opts!(
                "loadlab_io_fsyncs_total",
                "Cumulative fsync calls since the I/O controller was created."
            ))?,
            io_direct: gauge(opts!(
                "loadlab_io_direct",
                "Whether the I/O file was opened with O_DIRECT, bypassing the page cache. 1 or 0."
            ))?,
            io_read_latency: latency_histogram(
                "loadlab_io_read_latency_seconds",
                "Time spent in each pread call, in seconds.",
            )?,
            io_write_latency: latency_histogram(
                "loadlab_io_write_latency_seconds",
                "Time spent in each pwrite call, in seconds.",
            )?,
            io_fsync_latency: latency_histogram(
                "loadlab_io_fsync_latency_seconds",
                "Time spent in each fsync call, in seconds.",
            )?,
            io_converged: gauge(opts!(
                "loadlab_io_converged",
                "Whether every requested I/O rate is within tolerance of what was measured. 1 or 0."
            ))?,
            io_saturated: gauge(opts!(
                "loadlab_io_saturated",
                "Whether an operation cap or the safety bound stopped the byte target being \
                 reached. 1 or 0."
            ))?,
            configured_max_network_ops_per_second: gauge(opts!(
                "loadlab_configured_max_network_ops_per_second",
                "Ceiling the operator set on messages plus fresh connections per second. Separate \
                 from the other ceilings because network traffic leaves this host, so it is the one \
                 workload whose reach an operator has to cap."
            ))?,
            network_concurrency: gauge(opts!(
                "loadlab_network_concurrency",
                "Worker slots, and so concurrent TCP connections or UDP sockets, the generator is \
                 configured to keep."
            ))?,
            network_message_size_bytes: gauge(opts!(
                "loadlab_network_message_size_bytes",
                "Bytes in each message: one socket write (TCP) or datagram (UDP)."
            ))?,
            network_connections_per_second: float_gauge(
                "loadlab_network_connections_per_second",
                "Fresh TCP connections per second requested this tick. Zero means none, and the \
                 rate is not requested for UDP.",
            )?,
            network_connections_per_second_actual: float_gauge(
                "loadlab_network_connections_per_second_actual",
                "Fresh TCP connections per second measured. Compare with \
                 loadlab_network_connections_per_second to see the handshake and accept path's \
                 real limit.",
            )?,
            network_messages_per_second: float_gauge(
                "loadlab_network_messages_per_second",
                "Messages per second requested this tick. Zero means none.",
            )?,
            network_messages_per_second_actual: float_gauge(
                "loadlab_network_messages_per_second_actual",
                "Messages per second measured from the socket writes and sends that completed. \
                 Compare with loadlab_network_messages_per_second to see the stack's real limit.",
            )?,
            network_bytes_per_second_actual: float_gauge(
                "loadlab_network_bytes_per_second_actual",
                "Bytes per second measured, including the writes of fresh TCP connections.",
            )?,
            network_messages_total: counter(opts!(
                "loadlab_network_messages_total",
                "Cumulative messages sent over persistent connections (TCP) or datagrams (UDP) \
                 since the generator was created."
            ))?,
            network_bytes_total: counter(opts!(
                "loadlab_network_bytes_total",
                "Cumulative bytes the sockets accepted for sending since the generator was created."
            ))?,
            network_connections_total: counter(opts!(
                "loadlab_network_connections_total",
                "Cumulative fresh TCP connections established since the generator was created."
            ))?,
            network_errors_total: counter(opts!(
                "loadlab_network_errors_total",
                "Cumulative failed socket operations: connects, writes, and sends."
            ))?,
            network_converged: gauge(opts!(
                "loadlab_network_converged",
                "Whether every requested network rate is within tolerance of what was measured. \
                 1 or 0."
            ))?,
            network_saturated: gauge(opts!(
                "loadlab_network_saturated",
                "Whether the operator's operations-per-second ceiling stopped the requested rate \
                 being reached. 1 or 0."
            ))?,
            thp_requested: gauge_vec(
                opts!(
                    "loadlab_thp_requested",
                    "Transparent-huge-page policy requested by memory.thp. One series per policy; \
                     the active policy is 1 and the rest are 0."
                ),
                &["policy"],
            )?,
            thp_host: gauge_vec(
                opts!(
                    "loadlab_thp_host",
                    "Host transparent-huge-page setting from sysfs. One series per policy; the \
                     host's current value is 1 and the rest are 0. All zero when the setting is \
                     unavailable. Compare with loadlab_thp_requested to see a request the host \
                     cannot honour."
                ),
                &["policy"],
            )?,
            thp_host_enabled: gauge(opts!(
                "loadlab_thp_host_enabled",
                "Whether the host permits transparent huge pages at all (always or madvise). 1 or \
                 0. A request of always on a host that reports 0 here produces no huge pages, which \
                 is why the effective loadlab_memory_anon_huge_pages_bytes is the number to trust."
            ))?,
            seen: Mutex::new(Seen::default()),
        };

        let registry = &metrics.registry;
        for metric in [
            Box::new(metrics.build_info.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(metrics.process_uptime_seconds.clone()),
            Box::new(metrics.host_info.clone()),
            Box::new(metrics.experiment_seed.clone()),
            Box::new(metrics.experiment_elapsed_seconds.clone()),
            Box::new(metrics.process_threads.clone()),
            Box::new(metrics.workload_running.clone()),
            Box::new(metrics.configured_max_memory_bytes.clone()),
            Box::new(metrics.configured_max_file_backed_bytes.clone()),
            Box::new(metrics.configured_max_shared_bytes.clone()),
            Box::new(metrics.configured_max_io_bytes.clone()),
            Box::new(metrics.configured_tolerance_bytes.clone()),
            Box::new(metrics.configured_tick_ms.clone()),
            Box::new(metrics.controller_target_arena_bytes.clone()),
            Box::new(metrics.controller_arena_resident_bytes.clone()),
            Box::new(metrics.controller_convergence_error_bytes.clone()),
            Box::new(metrics.controller_converged.clone()),
            Box::new(metrics.controller_error.clone()),
            Box::new(metrics.controller_last_tick_age_seconds.clone()),
            Box::new(metrics.controller_ticks_total.clone()),
            Box::new(metrics.controller_reconcile_errors_total.clone()),
            Box::new(metrics.memory_baseline_rss_anon_bytes.clone()),
            Box::new(metrics.memory_rss_bytes.clone()),
            Box::new(metrics.memory_peak_rss_bytes.clone()),
            Box::new(metrics.memory_rss_anon_bytes.clone()),
            Box::new(metrics.memory_rss_file_bytes.clone()),
            Box::new(metrics.memory_rss_shmem_bytes.clone()),
            Box::new(metrics.memory_pss_bytes.clone()),
            Box::new(metrics.memory_swap_bytes.clone()),
            Box::new(metrics.memory_anon_huge_pages_bytes.clone()),
            Box::new(metrics.memory_anonymous_bytes.clone()),
            Box::new(metrics.memory_private_dirty_bytes.clone()),
            Box::new(metrics.faults_minor_total.clone()),
            Box::new(metrics.faults_major_total.clone()),
            Box::new(metrics.faults_generated_minor_total.clone()),
            Box::new(metrics.faults_generated_major_total.clone()),
            Box::new(metrics.faults_incidental_minor_total.clone()),
            Box::new(metrics.faults_incidental_major_total.clone()),
            Box::new(metrics.cpu_user_seconds_total.clone()),
            Box::new(metrics.cpu_system_seconds_total.clone()),
            Box::new(metrics.configured_cpu_workers.clone()),
            Box::new(metrics.configured_cpu_tolerance_percent.clone()),
            Box::new(metrics.cpu_measurement_resolution_percent.clone()),
            Box::new(metrics.cpu_workers.clone()),
            Box::new(metrics.cpu_worker_scheduling_available.clone()),
            Box::new(metrics.cpu_worker_runqueue_wait_nanoseconds.clone()),
            Box::new(metrics.cpu_worker_worst_runqueue_wait_nanoseconds.clone()),
            Box::new(metrics.cpu_capacity_percent.clone()),
            Box::new(metrics.cpu_requested_utilization_percent.clone()),
            Box::new(metrics.cpu_target_utilization_percent.clone()),
            Box::new(metrics.cpu_actual_utilization_percent.clone()),
            Box::new(metrics.cpu_duty_cycle.clone()),
            Box::new(metrics.cpu_converged.clone()),
            Box::new(metrics.cpu_saturated.clone()),
            Box::new(metrics.memory_access_target_pages_per_second.clone()),
            Box::new(metrics.memory_access_actual_pages_per_second.clone()),
            Box::new(metrics.memory_access_converged.clone()),
            Box::new(metrics.memory_access_saturated.clone()),
            Box::new(metrics.memory_access_minor_faults_rate.clone()),
            Box::new(metrics.memory_access_major_faults_rate.clone()),
            Box::new(metrics.file_backed_rss_bytes.clone()),
            Box::new(metrics.file_backed_pages.clone()),
            Box::new(metrics.file_backed_file_bytes.clone()),
            Box::new(metrics.file_backed_target_rss_bytes.clone()),
            Box::new(metrics.file_backed_requested_rss_bytes.clone()),
            Box::new(metrics.file_backed_converged.clone()),
            Box::new(metrics.file_backed_saturated.clone()),
            Box::new(metrics.file_backed_pages_per_second.clone()),
            Box::new(metrics.file_backed_actual_pages_per_second.clone()),
            Box::new(metrics.file_backed_minor_faults_rate.clone()),
            Box::new(metrics.file_backed_major_faults_rate.clone()),
            Box::new(metrics.shared_segment_bytes.clone()),
            Box::new(metrics.shared_private_bytes_per_actor.clone()),
            Box::new(metrics.shared_actors.clone()),
            Box::new(metrics.shared_child_actors.clone()),
            Box::new(metrics.shared_segment_physical_bytes.clone()),
            Box::new(metrics.shared_segment_rss_bytes.clone()),
            Box::new(metrics.shared_overcount_bytes.clone()),
            Box::new(metrics.shared_cow_physical_bytes.clone()),
            Box::new(metrics.shared_rss_bytes.clone()),
            Box::new(metrics.shared_pss_bytes.clone()),
            Box::new(metrics.shared_requested_cow_bytes.clone()),
            Box::new(metrics.shared_target_cow_bytes.clone()),
            Box::new(metrics.shared_converged.clone()),
            Box::new(metrics.shared_saturated.clone()),
            Box::new(metrics.io_region_bytes.clone()),
            Box::new(metrics.io_block_size_bytes.clone()),
            Box::new(metrics.io_queue_depth.clone()),
            Box::new(metrics.io_read_bps.clone()),
            Box::new(metrics.io_read_bps_actual.clone()),
            Box::new(metrics.io_write_bps.clone()),
            Box::new(metrics.io_write_bps_actual.clone()),
            Box::new(metrics.io_iops.clone()),
            Box::new(metrics.io_iops_actual.clone()),
            Box::new(metrics.io_fsync_rate.clone()),
            Box::new(metrics.io_fsyncs_per_second.clone()),
            Box::new(metrics.io_read_bytes_total.clone()),
            Box::new(metrics.io_write_bytes_total.clone()),
            Box::new(metrics.io_ops_total.clone()),
            Box::new(metrics.io_fsyncs_total.clone()),
            Box::new(metrics.io_direct.clone()),
            Box::new(metrics.io_read_latency.clone()),
            Box::new(metrics.io_write_latency.clone()),
            Box::new(metrics.io_fsync_latency.clone()),
            Box::new(metrics.io_converged.clone()),
            Box::new(metrics.io_saturated.clone()),
            Box::new(metrics.configured_max_network_ops_per_second.clone()),
            Box::new(metrics.network_concurrency.clone()),
            Box::new(metrics.network_message_size_bytes.clone()),
            Box::new(metrics.network_connections_per_second.clone()),
            Box::new(metrics.network_connections_per_second_actual.clone()),
            Box::new(metrics.network_messages_per_second.clone()),
            Box::new(metrics.network_messages_per_second_actual.clone()),
            Box::new(metrics.network_bytes_per_second_actual.clone()),
            Box::new(metrics.network_messages_total.clone()),
            Box::new(metrics.network_bytes_total.clone()),
            Box::new(metrics.network_connections_total.clone()),
            Box::new(metrics.network_errors_total.clone()),
            Box::new(metrics.network_converged.clone()),
            Box::new(metrics.network_saturated.clone()),
            Box::new(metrics.thp_requested.clone()),
            Box::new(metrics.thp_host.clone()),
            Box::new(metrics.thp_host_enabled.clone()),
        ] {
            registry.register(metric)?;
        }

        Ok(metrics)
    }

    /// Update every metric from a fresh reading and render the exposition.
    /// Publish which run these numbers belong to: the host, the seed and `t`.
    ///
    /// Split out of `render` because it is the one part of a scrape that answers "what was this?"
    /// rather than "how much?", and because `render` is long enough that five more lines there is
    /// five more lines every reader of it has to get past.
    fn set_run_identity(&self, state: &WorkloadState, runtime: &RuntimeStatus) {
        self.host_info.set(1);
        // Uptime and the experiment clock are deliberately separate series. They coincided while `t`
        // was measured from process start, and publishing one number for both would make a restarted
        // workload look like a slow-aging process.
        self.process_uptime_seconds
            .set(runtime.started_at.elapsed().as_secs_f64());
        self.experiment_seed
            .set(to_i64(state.experiment.seed.unwrap_or(0)));
        self.experiment_elapsed_seconds
            .set(runtime.experiment.elapsed_seconds());
    }

    /// The I/O latency histograms, for the controller that fills them.
    ///
    /// Handed over at startup rather than reached through the registry at scrape time: a scrape
    /// only reads counters, and the counts have to be incremented by the thread doing the I/O.
    #[must_use]
    pub fn io_latency_histograms(&self) -> IoLatencyHistograms {
        IoLatencyHistograms {
            read: self.io_read_latency.clone(),
            write: self.io_write_latency.clone(),
            fsync: self.io_fsync_latency.clone(),
        }
    }

    pub fn render(
        &self,
        state: &WorkloadState,
        runtime: &RuntimeStatus,
        memory: &MemoryStats,
        faults: &FaultStats,
        cpu: &CpuStats,
    ) -> Result<String, prometheus::Error> {
        self.build_info.set(1);
        self.set_run_identity(state, runtime);
        self.process_threads.set(to_i64(cpu.threads));
        self.workload_running.set(i64::from(state.running));
        self.configured_max_memory_bytes
            .set(to_i64(state.controller.max_memory_bytes));
        self.configured_max_file_backed_bytes
            .set(to_i64(runtime.file_backed_ceiling_bytes));
        self.configured_max_shared_bytes
            .set(to_i64(runtime.shared_ceiling_bytes));
        self.configured_max_io_bytes
            .set(to_i64(runtime.io_ceiling_bytes));
        self.configured_max_network_ops_per_second
            .set(to_i64(runtime.network_ceiling_ops_per_second));
        self.configured_tolerance_bytes
            .set(to_i64(state.controller.tolerance_bytes));
        self.configured_tick_ms
            .set(to_i64(state.controller.tick_ms));
        self.configured_cpu_workers
            .set(to_i64_from_usize(state.cpu.workers));
        self.configured_cpu_tolerance_percent.set(TOLERANCE_PCT);
        self.cpu_measurement_resolution_percent
            .set(measurement_resolution_percent(state.controller.tick_ms));

        self.controller_target_arena_bytes
            .set(to_i64(runtime.last_target_arena_bytes));
        self.controller_arena_resident_bytes
            .set(to_i64(runtime.arena_resident_bytes));
        // SAFETY of the cast: a byte delta bounded by the operator's ceiling, nowhere near 2^53.
        #[allow(clippy::cast_precision_loss)]
        self.controller_convergence_error_bytes
            .set(runtime.convergence_error_bytes as f64);
        self.controller_converged.set(i64::from(runtime.converged));
        self.controller_error
            .set(i64::from(runtime.last_error.is_some()));
        // `last_tick_at` is `None` before the first tick. Reporting 0 there would read as a
        // controller that just ticked, so this stays NaN until there is a tick to measure.
        self.controller_last_tick_age_seconds.set(
            runtime
                .last_tick_at
                .map_or(f64::NAN, |instant| instant.elapsed().as_secs_f64()),
        );

        self.memory_baseline_rss_anon_bytes
            .set(to_i64(runtime.baseline_rss_anon_bytes));
        self.memory_rss_bytes.set(to_i64(memory.rss_bytes));
        self.memory_peak_rss_bytes
            .set(to_i64(memory.peak_rss_bytes));
        self.memory_rss_anon_bytes
            .set(to_i64(memory.rss_anon_bytes));
        self.memory_rss_file_bytes
            .set(to_i64(memory.rss_file_bytes));
        self.memory_rss_shmem_bytes
            .set(to_i64(memory.rss_shmem_bytes));
        self.memory_pss_bytes.set(to_i64(memory.pss_bytes));
        self.memory_swap_bytes.set(to_i64(memory.swap_bytes));
        self.memory_anon_huge_pages_bytes
            .set(to_i64(memory.anon_huge_pages_bytes));
        self.memory_anonymous_bytes
            .set(to_i64(memory.anonymous_bytes));
        self.memory_private_dirty_bytes
            .set(to_i64(memory.private_dirty_bytes));

        self.cpu_workers
            .set(to_i64_from_usize(runtime.cpu.worker_count));
        self.cpu_worker_scheduling_available
            .set(i64::from(runtime.cpu.scheduling_available));
        // Published as a gauge rather than a counter, on purpose. The figures are cumulative per
        // worker thread, and the pool is torn down and rebuilt whenever `cpu.workers` or `cpu.cpus`
        // changes, so this value legitimately falls to zero and climbs again. Accumulated into a
        // counter it would freeze at its previous peak for as long as the new pool took to exceed
        // it, which is a metric that silently stops reporting. A gauge shows the restart; `deriv()`
        // over it still gives nanoseconds of wait per second.
        self.cpu_worker_runqueue_wait_nanoseconds
            .set(to_i64(runtime.cpu.worker_runqueue_wait_nanoseconds));
        self.cpu_worker_worst_runqueue_wait_nanoseconds
            .set(to_i64(runtime.cpu.worker_worst_runqueue_wait_nanoseconds));
        self.cpu_capacity_percent.set(runtime.cpu.capacity_pct);
        self.cpu_requested_utilization_percent
            .set(runtime.cpu.requested_utilization_pct);
        self.cpu_target_utilization_percent
            .set(runtime.cpu.target_utilization_pct);
        self.cpu_actual_utilization_percent
            .set(runtime.cpu.actual_utilization_pct);
        self.cpu_duty_cycle.set(runtime.cpu.duty_cycle);
        self.cpu_converged.set(i64::from(runtime.cpu.converged));
        self.cpu_saturated.set(i64::from(runtime.cpu.saturated));

        self.memory_access_target_pages_per_second
            .set(runtime.memory_access.pages_per_second);
        self.memory_access_actual_pages_per_second
            .set(runtime.memory_access.actual_pages_per_second);
        self.memory_access_converged
            .set(i64::from(runtime.memory_access.converged));
        self.memory_access_saturated
            .set(i64::from(runtime.memory_access.saturated));
        self.memory_access_minor_faults_rate
            .set(runtime.memory_access.minor_faults_rate);
        self.memory_access_major_faults_rate
            .set(runtime.memory_access.major_faults_rate);

        self.file_backed_rss_bytes
            .set(to_i64(runtime.file_backed.rss_bytes));
        self.file_backed_pages
            .set(to_i64(runtime.file_backed.pages));
        self.file_backed_file_bytes
            .set(to_i64(runtime.file_backed.file_bytes));
        self.file_backed_target_rss_bytes
            .set(to_i64(runtime.file_backed.target_rss_bytes));
        self.file_backed_requested_rss_bytes
            .set(to_i64(runtime.file_backed.requested_rss_bytes));
        self.file_backed_converged
            .set(i64::from(runtime.file_backed.converged));
        self.file_backed_saturated
            .set(i64::from(runtime.file_backed.saturated));
        self.file_backed_pages_per_second
            .set(runtime.file_backed.pages_per_second);
        self.file_backed_actual_pages_per_second
            .set(runtime.file_backed.actual_pages_per_second);
        self.file_backed_minor_faults_rate
            .set(runtime.file_backed.minor_faults_rate);
        self.file_backed_major_faults_rate
            .set(runtime.file_backed.major_faults_rate);

        let shared = &runtime.shared;
        self.shared_segment_bytes.set(to_i64(shared.shared_bytes));
        self.shared_private_bytes_per_actor
            .set(to_i64(shared.private_bytes_per_actor));
        self.shared_actors.set(to_i64(u64::from(shared.actors)));
        self.shared_child_actors
            .set(to_i64(u64::from(shared.child_actors)));
        self.shared_segment_physical_bytes
            .set(to_i64(shared.shared_physical_bytes));
        self.shared_segment_rss_bytes
            .set(to_i64(shared.shared_rss_bytes));
        self.shared_overcount_bytes
            .set(to_i64(shared.shared_overcount_bytes));
        self.shared_cow_physical_bytes
            .set(to_i64(shared.cow_physical_bytes));
        self.shared_rss_bytes.set(to_i64(shared.accounted_bytes));
        self.shared_pss_bytes.set(to_i64(shared.physical_bytes));
        self.shared_requested_cow_bytes
            .set(to_i64(shared.requested_cow_bytes));
        self.shared_target_cow_bytes
            .set(to_i64(shared.target_cow_bytes));
        self.shared_converged.set(i64::from(shared.converged));
        self.shared_saturated.set(i64::from(shared.saturated));

        let io = &runtime.io;
        self.io_region_bytes.set(to_i64(io.region_bytes));
        self.io_block_size_bytes.set(to_i64(io.block_size_bytes));
        self.io_queue_depth.set(to_i64_from_usize(io.queue_depth));
        self.io_read_bps.set(io.read_bps);
        self.io_read_bps_actual.set(io.read_bps_actual);
        self.io_write_bps.set(io.write_bps);
        self.io_write_bps_actual.set(io.write_bps_actual);
        self.io_iops.set(io.iops);
        self.io_iops_actual.set(io.iops_actual);
        self.io_fsync_rate.set(io.fsync_rate);
        self.io_fsyncs_per_second.set(io.fsyncs_per_second);
        self.io_direct.set(i64::from(io.direct));
        self.io_converged.set(i64::from(io.converged));
        self.io_saturated.set(i64::from(io.saturated));

        let network = &runtime.network;
        self.network_concurrency
            .set(to_i64_from_usize(network.concurrency));
        self.network_message_size_bytes
            .set(to_i64(network.message_size_bytes));
        self.network_connections_per_second
            .set(network.connections_per_second);
        self.network_connections_per_second_actual
            .set(network.connections_per_second_actual);
        self.network_messages_per_second
            .set(network.messages_per_second);
        self.network_messages_per_second_actual
            .set(network.messages_per_second_actual);
        self.network_bytes_per_second_actual
            .set(network.bytes_per_second_actual);
        self.network_converged.set(i64::from(network.converged));
        self.network_saturated.set(i64::from(network.saturated));

        // One series per policy so the set is stable across scrapes. The requested policy and the
        // host setting are separate series because they routinely disagree: the host is a fact and
        // the request may be one it cannot honour.
        for policy in crate::memory::ThpPolicy::ALL {
            let name = policy.as_str();
            self.thp_requested
                .with_label_values(&[name])
                .set(f64::from(u8::from(name == runtime.thp.requested)));
            self.thp_host
                .with_label_values(&[name])
                .set(f64::from(u8::from(name == runtime.thp.host)));
        }
        self.thp_host_enabled
            .set(i64::from(runtime.thp.host_enabled));

        // The counters are absolute readings, so each scrape contributes only its delta. See
        // `Seen` for why they are not exported as gauges.
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        advance(
            &self.faults_minor_total,
            &mut seen.minor_faults,
            faults.minor_faults,
        );
        advance(
            &self.faults_major_total,
            &mut seen.major_faults,
            faults.major_faults,
        );
        advance(
            &self.faults_generated_minor_total,
            &mut seen.generated_minor_faults,
            runtime.memory_access.generated_minor_faults,
        );
        advance(
            &self.faults_generated_major_total,
            &mut seen.generated_major_faults,
            runtime.memory_access.generated_major_faults,
        );
        advance(
            &self.faults_incidental_minor_total,
            &mut seen.incidental_minor_faults,
            runtime.memory_access.incidental_minor_faults,
        );
        advance(
            &self.faults_incidental_major_total,
            &mut seen.incidental_major_faults,
            runtime.memory_access.incidental_major_faults,
        );
        advance(
            &self.cpu_user_seconds_total,
            &mut seen.cpu_user,
            cpu.user_seconds,
        );
        advance(
            &self.cpu_system_seconds_total,
            &mut seen.cpu_system,
            cpu.system_seconds,
        );
        advance(
            &self.controller_ticks_total,
            &mut seen.ticks,
            runtime.tick_count,
        );
        advance(
            &self.controller_reconcile_errors_total,
            &mut seen.reconcile_errors,
            runtime.reconcile_errors_total,
        );
        advance(
            &self.io_read_bytes_total,
            &mut seen.io_read_bytes,
            io.read_bytes_total,
        );
        advance(
            &self.io_write_bytes_total,
            &mut seen.io_write_bytes,
            io.write_bytes_total,
        );
        advance(&self.io_ops_total, &mut seen.io_ops, io.ops_total);
        advance(&self.io_fsyncs_total, &mut seen.io_fsyncs, io.fsyncs_total);
        advance(
            &self.network_messages_total,
            &mut seen.network_messages,
            network.messages_total,
        );
        advance(
            &self.network_bytes_total,
            &mut seen.network_bytes,
            network.bytes_total,
        );
        advance(
            &self.network_connections_total,
            &mut seen.network_connections,
            network.connections_total,
        );
        advance(
            &self.network_errors_total,
            &mut seen.network_errors,
            network.errors_total,
        );
        drop(seen);

        let mut buffer = Vec::with_capacity(4096);
        TextEncoder::new().encode(&self.registry.gather(), &mut buffer)?;
        String::from_utf8(buffer).map_err(|error| prometheus::Error::Msg(error.to_string()))
    }
}

/// Fold an absolute reading into a Prometheus counter by contributing only the increase.
///
/// Every counter loadlab exports comes from a monotonic absolute source (`/proc` fault counts, the
/// controller's own tick count, process CPU time), but the exposition format only lets a counter
/// grow. Publishing the absolute value through `inc_by` would multiply the reading by the number of
/// scrapes, so the last published value is remembered and only the difference is added. That keeps
/// these real counters, which means `rate()` and `increase()` work on CPU; exporting them as
/// gauges would have forced `deriv()` on every dashboard.
///
/// A reading that went backwards (a counter wrap, or a `/proc` re-read across a fork) contributes
/// nothing rather than a negative delta, and re-baselines to the new value.
fn advance(counter: &impl CounterDelta, seen: &mut f64, current: impl Reading) {
    let current = current.reading();
    let delta = current - *seen;
    *seen = current;
    if delta > 0.0 {
        counter.add(delta);
    }
}

/// An absolute counter reading that `advance` can difference.
trait Reading {
    fn reading(self) -> f64;
}

impl Reading for f64 {
    fn reading(self) -> f64 {
        self
    }
}

impl Reading for u64 {
    // SAFETY of the cast: these are fault, tick and error counts. Reaching 2^53 would need more
    // ticks than the controller manages in several million years at 10 kHz.
    #[allow(clippy::cast_precision_loss)]
    fn reading(self) -> f64 {
        self as f64
    }
}

/// Lets `advance` work for both integer and float counters.
trait CounterDelta {
    fn add(&self, delta: f64);
}

impl CounterDelta for IntCounter {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn add(&self, delta: f64) {
        // `advance` only ever calls this with a strictly positive delta, and fault counts are
        // nowhere near `u64::MAX`, so the truncation is a floor and not a wrap.
        self.inc_by(delta as u64);
    }
}

impl CounterDelta for Counter {
    fn add(&self, delta: f64) {
        self.inc_by(delta);
    }
}

fn gauge(opts: prometheus::Opts) -> Result<IntGauge, prometheus::Error> {
    IntGauge::with_opts(opts)
}

fn gauge_vec(opts: prometheus::Opts, labels: &[&str]) -> Result<GaugeVec, prometheus::Error> {
    GaugeVec::new(opts, labels)
}

fn float_gauge(name: &str, help: &str) -> Result<Gauge, prometheus::Error> {
    Gauge::with_opts(Opts::new(name, help))
}

fn counter(opts: prometheus::Opts) -> Result<IntCounter, prometheus::Error> {
    IntCounter::with_opts(opts)
}

fn float_counter(opts: prometheus::Opts) -> Result<Counter, prometheus::Error> {
    Counter::with_opts(opts)
}

/// Shared handles to the three I/O latency histograms, handed to the I/O controller.
///
/// The controller records into these as each operation completes, rather than the scrape path
/// reconstructing a histogram from the snapshot. `Histogram::observe` takes `&self` because a
/// histogram keeps its counts in atomic cells, so this needs no lock and no channel: the workers
/// observe, the exposition reads the same counters.
///
/// This is why [`crate::io::LatencySummary`] and these histograms stay consistent: they are fed
/// from the same `Instant::now()` reading, one into a bucket count for the JSON and one into a
/// sample for Prometheus.
#[derive(Debug, Clone)]
pub struct IoLatencyHistograms {
    read: Histogram,
    write: Histogram,
    fsync: Histogram,
}

impl IoLatencyHistograms {
    /// Record one read, given its latency in nanoseconds.
    pub fn observe_read(&self, nanoseconds: u64) {
        self.read.observe(nanoseconds_to_seconds(nanoseconds));
    }

    /// Record one write, given its latency in nanoseconds.
    pub fn observe_write(&self, nanoseconds: u64) {
        self.write.observe(nanoseconds_to_seconds(nanoseconds));
    }

    /// Record one `fsync`, given its latency in nanoseconds.
    pub fn observe_fsync(&self, nanoseconds: u64) {
        self.fsync.observe(nanoseconds_to_seconds(nanoseconds));
    }
}

/// An I/O latency histogram over [`crate::io::LATENCY_BUCKETS_SECONDS`].
///
/// The bucket set is fixed at registration and shared by all three latency metrics, so the
/// exposition grows by a fixed number of series and never with traffic. Percentiles are read out of
/// these with `histogram_quantile`, which interpolates within a bucket; loadlab publishes no
/// percentile of its own for exactly that reason.
fn latency_histogram(name: &str, help: &str) -> Result<Histogram, prometheus::Error> {
    Histogram::with_opts(
        HistogramOpts::new(name, help).buckets(crate::io::LATENCY_BUCKETS_SECONDS.to_vec()),
    )
}

/// Nanoseconds to the seconds the histogram buckets are expressed in.
///
/// `f64` has 53 bits of mantissa, so this is exact for every nanosecond count a `u64` can hold
/// below 2^53 ns -- about 104 days -- and a plain `as f64` conversion is correct rather than
/// approximate for any latency that can appear in a sample.
fn nanoseconds_to_seconds(nanoseconds: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let seconds = nanoseconds as f64 / 1_000_000_000.0;
    debug_assert!(seconds.is_finite() && seconds >= 0.0);
    seconds
}

/// Saturate rather than wrap: `WorkloadState::validate` accepts a ceiling of `usize::MAX`, which is
/// wider than the `i64` a gauge can hold.
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The same, for the platform-sized counts the CPU configuration uses. `cpu.workers` is
/// validated against `MAX_WORKERS`, so this is a formality rather than a real risk.
fn to_i64_from_usize(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    // Every float compared in these tests is an exact integer value produced by a counter
    // delta or a byte count, so `==` is the correct comparison and an epsilon would only hide
    // a real arithmetic bug.
    #![allow(clippy::float_cmp)]

    /// A stand-in for the real host, so metric construction and the `loadlab_host_info` labels are
    /// testable without depending on the machine the tests run on.
    fn test_host() -> HostInfo {
        HostInfo {
            kernel_name: "Linux".to_owned(),
            kernel_release: "6.1.0".to_owned(),
            kernel_version: "#1 SMP".to_owned(),
            machine: "x86_64".to_owned(),
            cpus: 4,
            cpu_model: Some("Test CPU".to_owned()),
            memory_total_bytes: Some(8 * 1024 * 1024 * 1024),
            page_size_bytes: 4096,
        }
    }

    use std::{
        collections::HashSet,
        time::{Duration, Instant},
    };

    use super::Metrics;
    use crate::{
        procfs::HostInfo,
        procfs::{CpuStats, FaultStats, MemoryStats},
        state::{ControllerConfig, CpuRuntime, RuntimeStatus, WorkloadState},
    };

    fn memory() -> MemoryStats {
        MemoryStats {
            rss_bytes: 486_539_264,
            pss_bytes: 484_700_416,
            rss_anon_bytes: 482_195_968,
            rss_file_bytes: 3_407_872,
            rss_shmem_bytes: 0,
            anonymous_bytes: 482_195_968,
            private_clean_bytes: 2_097_152,
            private_dirty_bytes: 482_195_968,
            shared_clean_bytes: 1_441_792,
            shared_dirty_bytes: 0,
            swap_bytes: 0,
            anon_huge_pages_bytes: 0,
            peak_rss_bytes: 491_815_168,
        }
    }

    fn runtime() -> RuntimeStatus {
        let mut runtime = RuntimeStatus::new(131_072);
        runtime.last_tick_at = Instant::now().checked_sub(Duration::from_millis(60));
        runtime.tick_count = 3602;
        runtime.arena_resident_bytes = 482_062_336;
        runtime.last_target_arena_bytes = 484_076_929;
        runtime.convergence_error_bytes = 2_014_593;
        runtime.converged = true;
        runtime.reconcile_errors_total = 0;
        runtime.cpu = CpuRuntime {
            worker_count: 4,
            capacity_pct: 400.0,
            requested_utilization_pct: 100.0,
            target_utilization_pct: 100.0,
            actual_utilization_pct: 99.5,
            duty_cycle: 0.25,
            converged: true,
            saturated: false,
            scheduling_available: true,
            worker_runqueue_wait_nanoseconds: 7_000_000,
            worker_worst_runqueue_wait_nanoseconds: 3_000_000,
        };
        runtime
    }

    fn running_state() -> WorkloadState {
        WorkloadState {
            running: true,
            controller: ControllerConfig {
                max_memory_bytes: 900 * 1024 * 1024,
                ..ControllerConfig::default()
            },
            cpu: crate::state::CpuConfig {
                utilization: crate::setting::Setting::Expression("100".to_owned()),
                workers: 4,
                cpus: Vec::new(),
                affinity: "pin".to_owned(),
            },
            ..WorkloadState::default()
        }
    }

    fn exposition() -> String {
        Metrics::new(&test_host())
            .expect("registry")
            .render(
                &running_state(),
                &runtime(),
                &memory(),
                &FaultStats {
                    minor_faults: 118_256,
                    major_faults: 0,
                },
                &CpuStats {
                    user_seconds: 0.59,
                    system_seconds: 1.5,
                    threads: 6,
                },
            )
            .expect("encode")
    }

    /// The structural rules a Prometheus scraper relies on. The crate guarantees most of them, so
    /// this is here to catch a metric that is emitted with the wrong type or not emitted at all.
    fn validate(exposition: &str) {
        let mut declared: HashSet<String> = HashSet::new();
        let mut sampled: HashSet<String> = HashSet::new();

        for (number, line) in exposition.lines().enumerate() {
            let where_ = format!("line {}", number + 1);

            if line.is_empty() {
                continue;
            }

            if let Some(rest) = line.strip_prefix("# HELP ") {
                let name = rest.split(' ').next().expect("HELP needs a name");
                assert!(
                    declared.insert(name.to_owned()),
                    "{where_}: duplicate HELP for {name}"
                );
                continue;
            }

            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let mut parts = rest.split(' ');
                let name = parts.next().expect("TYPE needs a name");
                let kind = parts.next().expect("TYPE needs a kind");
                assert!(
                    ["counter", "gauge", "untyped", "histogram", "summary"].contains(&kind),
                    "{where_}: unknown metric type {kind:?}"
                );
                assert!(
                    declared.contains(name),
                    "{where_}: TYPE {name} declared before its HELP"
                );
                continue;
            }

            assert!(!line.starts_with('#'), "{where_}: unexpected comment");

            let (name, value) = split_sample(line)
                .unwrap_or_else(|| panic!("{where_}: sample {line:?} is not `name value`"));
            assert!(valid_value(value), "{where_}: bad value {value:?}");

            // A histogram declares HELP/TYPE on its family name but emits `_bucket`, `_sum` and
            // `_count` samples, none of which carry their own. Resolve the family so the "every
            // sample is declared" rule keeps meaning what it says.
            let family = if let Some(family) = histogram_family(&name) {
                assert!(
                    declared.contains(&family),
                    "{where_}: histogram sample {name} has no HELP/TYPE for {family}"
                );
                family
            } else {
                assert!(
                    declared.contains(&name),
                    "{where_}: sample {name} has no HELP/TYPE"
                );
                name
            };
            sampled.insert(family);
        }

        assert!(
            declared.is_subset(&sampled),
            "declared but never sampled: {:?}",
            declared.difference(&sampled).collect::<Vec<_>>()
        );
    }

    /// The family a histogram sample belongs to, if `name` is one of the three suffixes a
    /// histogram emits.
    fn histogram_family(name: &str) -> Option<String> {
        ["_bucket", "_sum", "_count"]
            .into_iter()
            .find_map(|suffix| name.strip_suffix(suffix).map(str::to_owned))
    }

    /// Split a sample into name and value, tolerating `{label="value"}`.
    fn split_sample(line: &str) -> Option<(String, &str)> {
        let after_name = match line.find('{') {
            Some(_) => {
                let close = line.find('}')?;
                &line[close + 1..]
            }
            None => &line[line.find(' ')?..],
        };
        let name_end = line.find(['{', ' '])?;
        Some((line[..name_end].to_owned(), after_name.trim()))
    }

    fn valid_value(value: &str) -> bool {
        if ["NaN", "+Inf", "-Inf"].contains(&value) {
            return true;
        }
        if value.contains("inf") || value.contains("nan") {
            return false;
        }
        value.parse::<f64>().is_ok()
    }

    fn sample_value(exposition: &str, name: &str) -> f64 {
        let value = exposition
            .lines()
            .find_map(|line| {
                let rest = line.strip_prefix(name)?;
                let after = match rest.find('}') {
                    Some(close) => rest[close + 1..].trim_start(),
                    None => rest.strip_prefix(' ')?,
                };
                (!after.is_empty()).then_some(after)
            })
            .unwrap_or_else(|| panic!("{name} is not exposed"));
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} has a non-numeric value {value:?}"))
    }

    #[test]
    fn exposition_is_structurally_valid() {
        validate(&exposition());
    }

    #[test]
    fn every_metric_the_docs_promise_is_exported() {
        let exposition = exposition();
        for name in [
            "loadlab_build_info",
            "loadlab_process_uptime_seconds",
            "loadlab_controller_last_tick_age_seconds",
            "loadlab_controller_ticks_total",
            "loadlab_controller_reconcile_errors_total",
            "loadlab_controller_error",
            "loadlab_workload_running",
            "loadlab_configured_max_memory_bytes",
            "loadlab_configured_max_file_backed_bytes",
            "loadlab_configured_tolerance_bytes",
            "loadlab_configured_tick_ms",
            "loadlab_controller_target_arena_bytes",
            "loadlab_controller_arena_resident_bytes",
            "loadlab_controller_convergence_error_bytes",
            "loadlab_controller_converged",
            "loadlab_memory_baseline_rss_anon_bytes",
            "loadlab_memory_rss_bytes",
            "loadlab_memory_peak_rss_bytes",
            "loadlab_memory_rss_anon_bytes",
            "loadlab_memory_pss_bytes",
            "loadlab_memory_swap_bytes",
            "loadlab_faults_minor_total",
            "loadlab_faults_major_total",
            "loadlab_process_cpu_user_seconds_total",
            "loadlab_process_cpu_system_seconds_total",
            "loadlab_process_threads",
            "loadlab_configured_cpu_workers",
            "loadlab_cpu_workers",
            "loadlab_cpu_capacity_percent",
            "loadlab_cpu_requested_utilization_percent",
            "loadlab_cpu_target_utilization_percent",
            "loadlab_cpu_actual_utilization_percent",
            "loadlab_cpu_duty_cycle",
            "loadlab_cpu_converged",
            "loadlab_cpu_saturated",
            "loadlab_memory_access_target_pages_per_second",
            "loadlab_memory_access_actual_pages_per_second",
            "loadlab_memory_access_converged",
            "loadlab_memory_access_saturated",
            "loadlab_memory_access_minor_faults_rate",
            "loadlab_memory_access_major_faults_rate",
            "loadlab_file_backed_rss_bytes",
            "loadlab_file_backed_pages",
            "loadlab_file_backed_file_bytes",
            "loadlab_file_backed_target_rss_bytes",
            "loadlab_file_backed_requested_rss_bytes",
            "loadlab_file_backed_converged",
            "loadlab_file_backed_saturated",
            "loadlab_file_backed_pages_per_second",
            "loadlab_file_backed_actual_pages_per_second",
            "loadlab_file_backed_minor_faults_rate",
            "loadlab_file_backed_major_faults_rate",
            "loadlab_configured_max_io_bytes",
            "loadlab_io_region_bytes",
            "loadlab_io_block_size_bytes",
            "loadlab_io_queue_depth",
            "loadlab_io_read_bps",
            "loadlab_io_read_bps_actual",
            "loadlab_io_write_bps",
            "loadlab_io_write_bps_actual",
            "loadlab_io_iops",
            "loadlab_io_iops_actual",
            "loadlab_io_fsync_rate",
            "loadlab_io_fsyncs_per_second",
            "loadlab_io_read_bytes_total",
            "loadlab_io_write_bytes_total",
            "loadlab_io_ops_total",
            "loadlab_io_fsyncs_total",
            "loadlab_io_converged",
            "loadlab_io_saturated",
            "loadlab_configured_max_network_ops_per_second",
            "loadlab_network_concurrency",
            "loadlab_network_message_size_bytes",
            "loadlab_network_connections_per_second",
            "loadlab_network_connections_per_second_actual",
            "loadlab_network_messages_per_second",
            "loadlab_network_messages_per_second_actual",
            "loadlab_network_bytes_per_second_actual",
            "loadlab_network_messages_total",
            "loadlab_network_bytes_total",
            "loadlab_network_connections_total",
            "loadlab_network_errors_total",
            "loadlab_network_converged",
            "loadlab_network_saturated",
        ] {
            // A sample is `name value` or `name{labels} value`, so compare the part before the
            // first space or brace. `loadlab_build_info` carries a label and would otherwise be
            // missed.
            let exported = exposition
                .lines()
                .any(|line| line.split([' ', '{']).next().unwrap_or_default() == name);
            assert!(exported, "{name} missing");
        }
    }

    #[test]
    fn counters_are_declared_as_counters_and_suffixed_total() {
        let exposition = exposition();
        for (number, line) in exposition.lines().enumerate() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let mut parts = rest.split(' ');
                let name = parts.next().expect("TYPE needs a name");
                let kind = parts.next().expect("TYPE needs a kind");
                // A histogram is cumulative too, but it reports its count through a `_count`
                // suffix that Prometheus defines, so `_total` would be wrong for it.
                if name.ends_with("_total") {
                    assert_eq!(
                        kind,
                        "counter",
                        "line {}: {name} ends in _total but is a {kind}",
                        number + 1
                    );
                } else if kind != "histogram" {
                    assert_eq!(
                        kind,
                        "gauge",
                        "line {}: {name} is a {kind} without a _total suffix",
                        number + 1
                    );
                }
            }
        }
    }

    #[test]
    fn absolute_counters_accumulate_their_delta_not_their_value() {
        // The whole reason `advance` exists. If the absolute reading were published directly, the
        // second scrape would report four times the ticks of the first.
        let metrics = Metrics::new(&test_host()).expect("registry");
        let faults = |minor| FaultStats {
            minor_faults: minor,
            major_faults: 0,
        };
        let cpu = CpuStats {
            user_seconds: 0.0,
            system_seconds: 0.0,
            threads: 6,
        };

        let first = metrics
            .render(&running_state(), &runtime(), &memory(), &faults(1000), &cpu)
            .expect("encode");
        let second = metrics
            .render(&running_state(), &runtime(), &memory(), &faults(1030), &cpu)
            .expect("encode");
        let third = metrics
            .render(&running_state(), &runtime(), &memory(), &faults(1030), &cpu)
            .expect("encode");

        // A Prometheus counter starts at zero when the process does, so the first scrape
        // legitimately publishes the whole reading (1000 faults since start). Later scrapes add
        // only what changed. Publishing the absolute value on every scrape instead would give
        // 1000, 2030, 3060 -- monotonically climbing without bound.
        assert_eq!(sample_value(&first, "loadlab_faults_minor_total"), 1000.0);
        assert_eq!(sample_value(&second, "loadlab_faults_minor_total"), 1030.0);
        assert_eq!(sample_value(&third, "loadlab_faults_minor_total"), 1030.0);
    }

    #[test]
    fn host_metadata_is_published_as_labels_on_a_single_series() {
        let metrics = Metrics::new(&test_host()).expect("registry");
        let cpu = CpuStats::default();
        let body = metrics
            .render(
                &running_state(),
                &runtime(),
                &memory(),
                &FaultStats::default(),
                &cpu,
            )
            .expect("encode");

        // The values a PromQL query joins on: machine, cores and model are what decide whether two
        // runs are comparable, and they are attributes of the series rather than separate series.
        // Labels in exposition order, which is alphabetical by name.
        assert!(
            body.contains(
                "loadlab_host_info{cpu_model=\"Test CPU\",cpus=\"4\",kernel_name=\"Linux\",\
                 kernel_release=\"6.1.0\",machine=\"x86_64\"} 1"
            ),
            "host info missing or relabelled:\n{body}"
        );
        // Exactly one, because one host per process. A second would mean two machines' numbers
        // could be told apart by series rather than by label, which defeats the point.
        assert_eq!(
            body.lines()
                .filter(|line| line.starts_with("loadlab_host_info{"))
                .count(),
            1
        );
    }

    #[test]
    fn a_cpu_model_loadlab_cannot_read_is_published_as_unknown_rather_than_empty() {
        let mut host = test_host();
        host.cpu_model = None;
        let metrics = Metrics::new(&host).expect("registry");
        let body = metrics
            .render(
                &running_state(),
                &runtime(),
                &memory(),
                &FaultStats::default(),
                &CpuStats::default(),
            )
            .expect("encode");

        // An empty label value is indistinguishable from a series that lost its labels, and
        // `cpu_model=""` matches nothing in a query written for a real model.
        assert!(
            body.contains("cpu_model=\"unknown\""),
            "a missing cpu model must still publish a label:\n{body}"
        );
    }

    #[test]
    fn the_experiment_seed_is_a_value_and_not_a_label() {
        let metrics = Metrics::new(&test_host()).expect("registry");
        let mut state = running_state();
        state.experiment.seed = Some(4242);
        let body = metrics
            .render(
                &state,
                &runtime(),
                &memory(),
                &FaultStats::default(),
                &CpuStats::default(),
            )
            .expect("encode");

        // The seed is client-supplied, so as a label every distinct seed would mint a series and
        // fragment the history behind it. As a gauge there is one series and the value changes.
        assert_eq!(sample_value(&body, "loadlab_experiment_seed"), 4242.0);
        assert!(
            !body.contains("loadlab_experiment_seed{"),
            "the seed must not be a label"
        );
    }

    #[test]
    fn no_configured_seed_is_reported_as_zero_rather_than_absent() {
        let metrics = Metrics::new(&test_host()).expect("registry");
        let mut state = running_state();
        state.experiment.seed = None;
        let body = metrics
            .render(
                &state,
                &runtime(),
                &memory(),
                &FaultStats::default(),
                &CpuStats::default(),
            )
            .expect("encode");

        // 0 and not `null`: a gauge cannot carry null, and a series that appears only once a seed
        // is set cannot answer `seed == 0` for the runs that had none.
        assert_eq!(sample_value(&body, "loadlab_experiment_seed"), 0.0);
    }

    #[test]
    fn the_experiment_elapsed_series_tracks_the_run_and_not_the_process() {
        let metrics = Metrics::new(&test_host()).expect("registry");
        let mut status = runtime();
        // A run that started a moment ago, on a process that has been up for hours: the whole
        // reason the experiment clock exists.
        let mut state = running_state();
        state.experiment.seed = Some(1);

        metrics
            .render(
                &state,
                &status,
                &memory(),
                &FaultStats::default(),
                &CpuStats::default(),
            )
            .expect("encode");
        let before = sample_value(
            &metrics
                .render(
                    &state,
                    &status,
                    &memory(),
                    &FaultStats::default(),
                    &CpuStats::default(),
                )
                .expect("encode"),
            "loadlab_experiment_elapsed_seconds",
        );
        status.experiment.observe(true, None);
        std::thread::sleep(Duration::from_millis(20));
        let body = metrics
            .render(
                &state,
                &status,
                &memory(),
                &FaultStats::default(),
                &CpuStats::default(),
            )
            .expect("encode");

        let after = sample_value(&body, "loadlab_experiment_elapsed_seconds");
        assert!(after > before, "the experiment clock did not advance");
        assert!(
            after < status.started_at.elapsed().as_secs_f64(),
            "the experiment clock should be the age of the run, not of the process"
        );
    }

    #[test]
    fn a_counter_reading_that_goes_backwards_is_ignored_not_negated() {
        let metrics = Metrics::new(&test_host()).expect("registry");
        let cpu = CpuStats {
            user_seconds: 5.0,
            system_seconds: 0.0,
            threads: 6,
        };
        let no_faults = FaultStats::default();

        metrics
            .render(&running_state(), &runtime(), &memory(), &no_faults, &cpu)
            .expect("encode");
        // A reading below the baseline must not push the counter negative.
        let regressed = metrics.render(
            &running_state(),
            &runtime(),
            &memory(),
            &no_faults,
            &CpuStats {
                user_seconds: 1.0,
                ..cpu
            },
        );
        assert!(regressed.is_ok(), "encoding must not fail on a regression");
        // The first scrape published 5.0; the 1.0 reading must neither subtract from it nor make
        // it negative.
        assert_eq!(
            sample_value(
                &regressed.unwrap(),
                "loadlab_process_cpu_user_seconds_total"
            ),
            5.0,
            "a backwards reading must not lower the counter"
        );
    }

    #[test]
    fn a_tick_that_never_happened_exports_nan_not_a_bogus_zero() {
        let runtime = RuntimeStatus::new(0);
        let body = Metrics::new(&test_host())
            .expect("registry")
            .render(
                &WorkloadState::default(),
                &runtime,
                &MemoryStats::default(),
                &FaultStats::default(),
                &CpuStats::default(),
            )
            .expect("encode");
        assert!(
            body.contains("loadlab_controller_last_tick_age_seconds NaN"),
            "an unset tick should be NaN so it does not read as a healthy zero age:\n{body}"
        );
        assert!(body.contains("loadlab_controller_error 0"));
        assert!(body.contains("loadlab_workload_running 0"));
    }

    #[test]
    fn a_controller_error_is_visible_as_a_flag_and_has_survived_in_the_counter() {
        let mut runtime = RuntimeStatus::new(0);
        runtime.reconcile_errors_total = 7;
        runtime.last_error = Some("madvise failed".to_owned());
        let body = Metrics::new(&test_host())
            .expect("registry")
            .render(
                &WorkloadState::default(),
                &runtime,
                &MemoryStats::default(),
                &FaultStats::default(),
                &CpuStats::default(),
            )
            .expect("encode");
        assert!(body.contains("loadlab_controller_error 1"));
        assert!(body.contains("loadlab_controller_reconcile_errors_total 7"));
        validate(&body);
    }

    #[test]
    fn label_values_are_escaped() {
        // The reason this module delegates to the crate instead of formatting text by hand. A
        // label value containing a quote or a backslash has to come back escaped, or the scrape
        // output stops parsing and the whole target goes silent rather than just being wrong.
        use prometheus::{Encoder, IntGauge, Opts, Registry, TextEncoder};

        let registry = Registry::new();
        let probe = IntGauge::with_opts(
            Opts::new("loadlab_escape_probe", "help").const_label("expr", r#"a"b\c"#),
        )
        .expect("probe");
        probe.set(1);
        registry.register(Box::new(probe)).expect("register");

        let mut buffer = Vec::new();
        TextEncoder::new()
            .encode(&registry.gather(), &mut buffer)
            .expect("encode");
        let body = String::from_utf8(buffer).expect("utf-8");

        assert!(
            body.contains(r#"loadlab_escape_probe{expr="a\"b\\c"} 1"#),
            "label value was not escaped:\n{body}"
        );
        validate(&body);
    }

    #[test]
    fn a_ceiling_wider_than_a_gauge_saturates_instead_of_wrapping() {
        // `WorkloadState::validate` accepts `usize::MAX`, which does not fit the i64 behind a gauge.
        let mut state = running_state();
        state.controller.max_memory_bytes = u64::MAX;
        let body = Metrics::new(&test_host())
            .expect("registry")
            .render(
                &state,
                &runtime(),
                &memory(),
                &FaultStats::default(),
                &CpuStats::default(),
            )
            .expect("encode");
        let exported = body
            .lines()
            .find_map(|line| line.strip_prefix("loadlab_configured_max_memory_bytes "))
            .expect("ceiling exported")
            .trim()
            .parse::<f64>()
            .expect("numeric");
        // The renderer formats integers through f64, so i64::MAX comes back rounded rather than
        // exact. What matters is that it saturates near the top of the range instead of wrapping
        // to a negative number, which is what a bare `as i64` would have produced.
        assert!(
            exported > 9.0e18,
            "expected saturation near i64::MAX, got {exported}"
        );
    }

    /// The docs tell operators to alert on
    /// `loadlab_controller_last_tick_age_seconds > 5 * tick_ms / 1000`. That rule is only
    /// writable in `PromQL` if the tick interval is exported, so pin the contract: the gauge tracks
    /// the configured interval, and the staleness threshold is five times it.
    #[test]
    fn the_configured_tick_is_exported_so_the_staleness_rule_is_writable() {
        let mut state = running_state();
        state.controller.tick_ms = 250;
        let body = Metrics::new(&test_host())
            .expect("registry")
            .render(
                &state,
                &runtime(),
                &memory(),
                &FaultStats::default(),
                &CpuStats::default(),
            )
            .expect("encode");
        let tick_ms = sample_value(&body, "loadlab_configured_tick_ms");
        assert_eq!(tick_ms, 250.0);

        // The rule the docs publish, evaluated as a PromQL expression would.
        let staleness_limit = 5.0 * tick_ms / 1000.0;
        assert!((staleness_limit - 1.25).abs() < f64::EPSILON);

        // And the staleness gauge has to be comparable against it, so it must be seconds.
        let age = sample_value(&body, "loadlab_controller_last_tick_age_seconds");
        assert!(age >= 0.0 && age < staleness_limit, "fresh tick: age={age}");
    }
}
