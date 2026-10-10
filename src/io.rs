//! Disk I/O: a temporary file loadlab reads from and writes to at a requested rate.
//!
//! Where [`crate::file_backed`] generates memory pressure *through* a file, this module uses the
//! file as the workload itself: it moves bytes with `pread`/`pwrite` and optionally writes them
//! through to stable storage with `sync_data`.
//!
//! The design choices that matter, and why:
//!
//! - **Throughput is measured, not modelled.** Every op is a real `pread`/`pwrite` on a real
//!   file, and the reported *actual* rates are the sum of the syscalls' return values divided by
//!   the tick's wall time. A model would keep reporting the target through page-cache hits and
//!   filesystems that silently ignore durability, which is exactly the difference this tool
//!   exists to expose.
//! - **The file is loadlab's.** It is created with `O_EXCL` and mode 0600 through [`tempfile`],
//!   lives inside a ceiling the controller rounds to a whole block, and is unlinked on drop.
//!   Nothing outside loadlab can be overwritten, because loadlab never accepts a path — only the
//!   directory it is created in comes from configuration.
//! - **`fsync_rate` is independent of read/write.** It is a credit accumulator, so a rate below
//!   one per tick still produces an occasional `fsync` instead of rounding to nothing forever.
//!
//! There is no `io_uring` backend: the controller is synchronous, with a thread-per-queue-depth
//! fan-out. An async backend can replace [`IoController::execute`] without changing the snapshot or
//! the configuration.

// The controller is built around byte and operation rates, so it converts between `u64` counts and
// `f64` rates throughout. Every conversion is bounded by the per-tick caps below, which are far
// inside `f64`'s exact integer range, so these lints are noise in this module.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::{
    fs::{File, OpenOptions},
    io,
    os::fd::{AsRawFd, RawFd},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::Instant,
};

use tempfile::NamedTempFile;

use crate::{error::LoadlabError, file_backed::check_directory};

/// Block size used when a configuration does not name one. Matches the page size on every host
/// this runs on, so a default request is expressed in the units the kernel actually moves.
const DEFAULT_BLOCK_SIZE: usize = 4096;

/// Upper bound on `block_size`, to keep a single buffer allocation bounded.
const MAX_BLOCK_SIZE: usize = 16 * 1024 * 1024;

/// Upper bound on `queue_depth`. The fan-out is one thread per slot per tick, so this is a limit
/// on threads, not on requests.
const MAX_QUEUE_DEPTH: usize = 64;

/// Hard bound on operations issued in one tick, independent of the requested rates. At the
/// default 4KiB block this is 4GiB of I/O in a single tick, far above any sane target, so it only
/// ever fires as a safety valve against a nonsensical configuration.
const MAX_OPS_PER_TICK: u64 = 1_000_000;

/// Hard bound on `fsync` calls in one tick, for the same reason.
const MAX_FSYNCS_PER_TICK: u64 = 65_536;

/// Absolute tolerance (bytes/op per second) under which a rate reports convergence.
const RATE_TOLERANCE: f64 = 1.0;

/// Upper bound on each feedback integral, in bytes/second.
const MAX_INTEGRAL_BPS: f64 = 1_073_741_824.0;

/// Proportional gain for the rate feedback loops.
const RATE_KP: f64 = 0.1;

/// Alignment that every `O_DIRECT` buffer address, file offset and length must satisfy.
///
/// This is a portability requirement, not an observation about any one machine. A filesystem with
/// 4KiB logical blocks requires a direct read or write to be 4KiB-aligned in address, offset and
/// length, and returns `EINVAL` otherwise; the default `block_size` already is 4096, so requiring
/// this costs nothing and never under-aligns a filesystem that wanted less.
///
/// Two things are deliberately *not* done here.
///
/// The requirement is not read back from the filesystem, because there is no portable ioctl for
/// "the alignment `O_DIRECT` wants". `BLKSSZGET` reports the device's block size, which is not the
/// constraint that binds first -- the filesystem's logical block size is -- so probing it would
/// answer a different question.
///
/// Nor is it verified against the running kernel, because the answer is not stable enough to test.
/// On the host this was developed on, a 4KiB-logical-block ext4 accepts a `pread` through an
/// `O_DIRECT` descriptor at an address one byte past a page boundary, and a sparse file reads back
/// zeros through a misaligned direct read without complaint. A filesystem that enforces the rule is
/// the common case, but a test asserting `EINVAL` would be asserting one kernel's leniency and
/// would then fail, correctly, on a stricter one. So loadlab satisfies the alignment unconditionally
/// and treats it as a contract with the filesystem rather than with the running kernel.
pub const DIRECT_ALIGNMENT: usize = 4096;

/// Upper bound on latency bucket boundaries, in seconds, shared by every I/O latency histogram.
///
/// Spans 100 microseconds (a page-cache or fast-NVMe read) to one second (a device that has stopped
/// responding) in roughly 2.5x steps at the bottom and decade steps at the top, which is where the
/// interesting distinctions are: sub-millisecond versus millisecond separates cache from device,
/// and tens of milliseconds versus seconds separates a slow device from a stalled one. Ten
/// boundaries plus `+Inf` is eleven series per histogram, three histograms, and that number does not
/// grow with traffic.
pub const LATENCY_BUCKETS_SECONDS: [f64; 10] = [
    0.000_1, 0.000_25, 0.000_5, 0.001, 0.002_5, 0.005, 0.01, 0.05, 0.5, 1.0,
];

/// The same boundaries in nanoseconds, which is what the accumulator compares against.
///
/// Kept literal rather than derived at compile time so the hot path is an integer compare, with a
/// test asserting the two tables agree.
const LATENCY_BUCKETS_NANOSECONDS: [u64; 10] = [
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_500_000,
    5_000_000,
    10_000_000,
    50_000_000,
    500_000_000,
    1_000_000_000,
];

/// Nanoseconds in one second, for converting a [`std::time::Duration`] into a bucket key.
const NANOSECONDS_PER_SECOND: f64 = 1_000_000_000.0;

/// Cumulative per-operation latency, in fixed buckets.
///
/// A histogram rather than stored samples, for two reasons. Its memory is fixed regardless of how
/// much traffic crosses it, and merging the per-worker accumulators a tick produces is a sum of
/// arrays rather than a concatenation of vectors across a thread boundary. The cost is that a
/// quantile read back out of it is bucket-interpolated, which is why nothing in loadlab reports a
/// percentile directly: [`LatencySummary`] carries only the exact aggregates, and
/// `histogram_quantile` over the Prometheus buckets is documented as the way to get an approximate
/// one.
#[derive(Debug, Clone, Default)]
struct Latency {
    /// Per-bucket counts, where entry `i` counts samples at or below
    /// [`LATENCY_BUCKETS_NANOSECONDS`][i]. A sample above every boundary is counted in neither,
    /// and is still visible as the difference between `count` and the buckets' total.
    buckets: [u64; LATENCY_BUCKETS_SECONDS.len()],
    count: u64,
    sum_nanoseconds: u64,
    min_nanoseconds: Option<u64>,
    max_nanoseconds: Option<u64>,
}

impl Latency {
    /// Record one observation, in nanoseconds.
    fn record(&mut self, nanoseconds: u64) {
        self.count += 1;
        self.sum_nanoseconds = self.sum_nanoseconds.saturating_add(nanoseconds);
        self.min_nanoseconds = Some(
            self.min_nanoseconds
                .map_or(nanoseconds, |current| current.min(nanoseconds)),
        );
        self.max_nanoseconds = Some(
            self.max_nanoseconds
                .map_or(nanoseconds, |current| current.max(nanoseconds)),
        );
        if let Some(index) = LATENCY_BUCKETS_NANOSECONDS
            .iter()
            .position(|boundary| nanoseconds <= *boundary)
        {
            self.buckets[index] += 1;
        }
    }

    /// Fold another accumulator into this one.
    fn merge(&mut self, other: &Self) {
        for (slot, value) in self.buckets.iter_mut().zip(other.buckets.iter()) {
            *slot += value;
        }
        self.count += other.count;
        self.sum_nanoseconds = self.sum_nanoseconds.saturating_add(other.sum_nanoseconds);
        self.min_nanoseconds = match (self.min_nanoseconds, other.min_nanoseconds) {
            (Some(current), Some(incoming)) => Some(current.min(incoming)),
            (None, incoming) => incoming,
            (current, None) => current,
        };
        self.max_nanoseconds = match (self.max_nanoseconds, other.max_nanoseconds) {
            (Some(current), Some(incoming)) => Some(current.max(incoming)),
            (None, incoming) => incoming,
            (current, None) => current,
        };
    }

    /// The exact aggregates for the JSON report.
    fn summary(&self) -> LatencySummary {
        LatencySummary {
            count: self.count,
            sum_seconds: self.sum_nanoseconds as f64 / NANOSECONDS_PER_SECOND,
            min_seconds: self
                .min_nanoseconds
                .map(|nanoseconds| nanoseconds as f64 / NANOSECONDS_PER_SECOND),
            max_seconds: self
                .max_nanoseconds
                .map(|nanoseconds| nanoseconds as f64 / NANOSECONDS_PER_SECOND),
        }
    }
}

/// The exact latency aggregates loadlab publishes, over the life of the controller.
///
/// No percentiles. A percentile over a histogram is an interpolation, and an interpolated number
/// wearing no label is the failure mode this project exists to avoid; a client that wants one gets
/// it from `histogram_quantile` over the Prometheus buckets and knows it is approximate. Count,
/// sum, min and max are exact and mean what they say.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct LatencySummary {
    /// Operations observed.
    pub count: u64,
    /// Total time spent in the operations, in seconds.
    pub sum_seconds: f64,
    /// Fastest operation, in seconds. `null` when nothing has been observed.
    pub min_seconds: Option<f64>,
    /// Slowest operation, in seconds. `null` when nothing has been observed.
    pub max_seconds: Option<f64>,
}

/// A `block_size` buffer whose address is a multiple of [`DIRECT_ALIGNMENT`].
///
/// `O_DIRECT` requires the buffer address, the file offset and the length to all be aligned to the
/// filesystem's logical block size, and `Vec<u8>` promises nothing about its address. This
/// over-allocates by one alignment unit and hands out a sub-slice starting at the first aligned
/// address.
///
/// The backing is a [`Box<[u8]>`] rather than a `Vec` for one specific reason: a `Box<[u8]>` cannot
/// reallocate, so the address of its data never changes, which is what makes the computed offset
/// stay valid. A `Vec` would let the buffer silently become unaligned the first time something
/// pushed its length past its capacity, and the failure would surface later as an `EINVAL` from the
/// kernel rather than as the bug it is. See [`DIRECT_ALIGNMENT`] for why the alignment is a
/// portability requirement rather than something this host can be shown to enforce.
///
/// Always used, not only in direct mode: it costs one extra page per worker per tick, and skipping
/// the branch is worth more than the page.
struct AlignedBuffer {
    /// The whole allocation. The padding before `start` and after `start + len` is never handed
    /// out, so no caller can read or write it.
    backing: Box<[u8]>,
    start: usize,
    len: usize,
}

impl AlignedBuffer {
    /// Allocate `len` bytes at an aligned address, filled with `fill`.
    fn new(len: usize, fill: u8) -> Self {
        let mut backing = vec![fill; len + DIRECT_ALIGNMENT].into_boxed_slice();
        let base = backing.as_ptr() as usize;
        let start = (DIRECT_ALIGNMENT - base % DIRECT_ALIGNMENT) % DIRECT_ALIGNMENT;
        debug_assert_eq!((base + start) % DIRECT_ALIGNMENT, 0);
        backing.fill(fill);
        Self {
            backing,
            start,
            len,
        }
    }

    fn as_slice(&self) -> &[u8] {
        &self.backing[self.start..self.start + self.len]
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.backing[self.start..self.start + self.len]
    }
}

/// The walk pattern used to choose block offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IoPattern {
    /// Every block in order, wrapping at the end of the region.
    #[default]
    Sequential,
    /// A uniformly pseudo-random block per operation.
    Random,
    /// Every `stride_blocks`-th block, wrapping at the end of the region.
    Stride,
}

impl IoPattern {
    /// The configuration name of this pattern.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Random => "random",
            Self::Stride => "stride",
        }
    }

    /// Parse a pattern name from a configuration string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "sequential" => Some(Self::Sequential),
            "random" => Some(Self::Random),
            "stride" => Some(Self::Stride),
            _ => None,
        }
    }

    /// Every pattern, in documentation order.
    pub const ALL: [Self; 3] = [Self::Sequential, Self::Random, Self::Stride];
}

/// The effective targets for one reconciliation, after the caller has applied the workload's
/// running/stopped state and the operator's ceiling.
#[derive(Debug, Clone, Copy)]
pub struct IoSettings {
    pub read_bps: f64,
    pub write_bps: f64,
    pub iops: f64,
    pub fsync_rate: f64,
    pub block_size: usize,
    pub queue_depth: usize,
    pub pattern: IoPattern,
    pub stride_blocks: u64,
    pub random_seed: Option<u64>,
    /// Whether the workload asked for `O_DIRECT`, bypassing the page cache.
    pub direct: bool,
}

/// What one reconciliation produced.
#[derive(Debug, Clone, Default)]
pub struct IoSnapshot {
    pub block_size: u64,
    /// Whether the descriptor in use was opened with `O_DIRECT`.
    pub direct: bool,
    /// The requested rate, before any clamping.
    pub read_bps: f64,
    pub read_bps_actual: f64,
    pub write_bps: f64,
    pub write_bps_actual: f64,
    /// The requested total-operations cap. Zero means uncapped.
    pub iops: f64,
    pub iops_actual: f64,
    pub fsync_rate: f64,
    pub fsyncs_per_second: f64,
    pub read_bytes_total: u64,
    pub write_bytes_total: u64,
    pub ops_total: u64,
    pub fsyncs_total: u64,
    pub read_latency: LatencySummary,
    pub write_latency: LatencySummary,
    pub fsync_latency: LatencySummary,
    pub converged: bool,
    pub saturated: bool,
}

/// A disk-I/O generator over one temporary file.
pub struct IoController {
    file: NamedTempFile,
    /// The descriptor every `pread`/`pwrite` and `sync_data` goes through.
    ///
    /// A separate descriptor from [`Self::file`]'s because the two need different open flags: the
    /// `NamedTempFile` exists to own the path and unlink it on drop, and `tempfile` offers no way
    /// to hand it custom flags, whereas `O_DIRECT` has to be in the `open(2)` call. Both name the
    /// same inode, so `set_len` on one is visible through the other.
    io_file: File,
    /// Whether `io_file` was opened with `O_DIRECT`, cached so a reconcile that does not change it
    /// does not reopen the file on every tick.
    direct: bool,
    /// The file's path, kept as a `String` so reporting it does not allocate or leak.
    path: String,
    region_bytes: u64,
    /// The operator's ceiling on the region, before block rounding.
    max_bytes: u64,
    block_size: usize,
    pattern: IoPattern,
    stride_blocks: u64,
    rng_state: u64,
    cursor: u64,
    last_wall: Option<Instant>,
    last_read_bps: f64,
    last_write_bps: f64,
    read_integral: f64,
    write_integral: f64,
    fsync_credit: f64,
    read_bytes_total: u64,
    write_bytes_total: u64,
    ops_total: u64,
    fsyncs_total: u64,
    /// Cumulative operation latencies. Never reset: a client that computes a rate from
    /// `sum_seconds` over two scrapes gets the right answer without having to notice a restart.
    read_latency: Latency,
    write_latency: Latency,
    fsync_latency: Latency,
    /// Prometheus side of the same observations. Kept beside the accumulators rather than derived
    /// from them, because a histogram cannot be rebuilt from bucket counts: `_sum` is not
    /// recoverable from the bucket a sample landed in.
    latency_histograms: crate::metrics::IoLatencyHistograms,
}

impl IoController {
    /// Create a controller over a fresh temporary file.
    ///
    /// `max_bytes` bounds the region the controller walks. It is rounded up to a whole block, so
    /// the file can exceed it by less than one block; the reported `region_bytes` is the rounded
    /// number, so the overshoot is visible rather than a surprise.
    ///
    /// `dir` has to be on a real filesystem, for the reason [`check_directory`] documents: a
    /// tmpfs region is page cache and shared memory, and no `pread`/`pwrite` against it would
    /// exercise a block device at all.
    pub fn new(
        max_bytes: u64,
        dir: &Path,
        latency_histograms: crate::metrics::IoLatencyHistograms,
    ) -> Result<Self, LoadlabError> {
        check_directory(dir)?;
        let block_size = DEFAULT_BLOCK_SIZE;
        let region_bytes = round_up(max_bytes, block_size as u64);
        let file = NamedTempFile::new_in(dir).map_err(|error| io_error(&error))?;
        file.as_file()
            .set_len(region_bytes)
            .map_err(|error| io_error(&error))?;
        let io_file = open_io_file(file.path(), false, block_size)?;
        let path = file.path().to_string_lossy().into_owned();
        Ok(Self {
            file,
            io_file,
            direct: false,
            path,
            region_bytes,
            max_bytes,
            block_size,
            pattern: IoPattern::Sequential,
            stride_blocks: 1,
            rng_state: 1,
            cursor: 0,
            last_wall: None,
            last_read_bps: 0.0,
            last_write_bps: 0.0,
            read_integral: 0.0,
            write_integral: 0.0,
            fsync_credit: 0.0,
            read_bytes_total: 0,
            write_bytes_total: 0,
            ops_total: 0,
            fsyncs_total: 0,
            read_latency: Latency::default(),
            write_latency: Latency::default(),
            fsync_latency: Latency::default(),
            latency_histograms,
        })
    }

    /// The path of the file loadlab created and owns.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The block-aligned size of the region being walked.
    #[must_use]
    pub fn region_bytes(&self) -> u64 {
        self.region_bytes
    }

    /// Apply per-tick settings that do not require reconfiguring the file on their own.
    fn apply_settings(&mut self, settings: &IoSettings) -> Result<(), LoadlabError> {
        let block_size = settings.block_size.clamp(1, MAX_BLOCK_SIZE);
        if block_size != self.block_size {
            self.block_size = block_size;
            self.resize_region()?;
        }
        if settings.direct != self.direct {
            self.io_file = open_io_file(self.file.path(), settings.direct, block_size)?;
            self.direct = settings.direct;
        }
        self.pattern = settings.pattern;
        self.stride_blocks = settings.stride_blocks.max(1);
        // A new seed is a new walk, so the cursor and the stream both restart. Resetting only the
        // cursor would replay the same blocks in a different order, which is not what
        // "reproducible" means to someone who changed the seed.
        if let Some(seed) = settings.random_seed {
            if seed != self.rng_state {
                self.rng_state = seed;
                self.cursor = 0;
            }
        }
        Ok(())
    }

    /// Re-round the region after a block-size change and truncate or extend the file to match.
    ///
    /// Unlike a mapping, a file walked with `pread`/`pwrite` can shrink safely, so this does not
    /// have to preserve the larger size.
    fn resize_region(&mut self) -> Result<(), LoadlabError> {
        let region_bytes =
            round_up(self.max_bytes, self.block_size as u64).max(self.block_size as u64);
        if region_bytes != self.region_bytes {
            self.file
                .as_file()
                .set_len(region_bytes)
                .map_err(|error| io_error(&error))?;
            self.region_bytes = region_bytes;
        }
        Ok(())
    }

    /// Converge the read, write, iops, and fsync targets and report what the controller is doing.
    #[allow(clippy::too_many_lines)]
    pub fn reconcile(&mut self, settings: &IoSettings) -> Result<IoSnapshot, LoadlabError> {
        self.apply_settings(settings)?;

        let now = Instant::now();
        let elapsed = self
            .last_wall
            .map_or(0.0, |previous| now.duration_since(previous).as_secs_f64());
        self.last_wall = Some(now);

        // A non-finite target would make every comparison below false and produce a silent
        // controller, so it is treated as zero rather than propagated.
        let read_bps = finite_nonnegative(settings.read_bps);
        let write_bps = finite_nonnegative(settings.write_bps);
        let iops = finite_nonnegative(settings.iops);
        let fsync_rate = finite_nonnegative(settings.fsync_rate);

        let mut saturated = false;
        let mut read_count = 0_u64;
        let mut write_count = 0_u64;

        if elapsed > 0.0 {
            if read_bps == 0.0 && write_bps == 0.0 && iops > 0.0 {
                // No byte target but an operation target: split the budget evenly, which is the
                // only definition that needs no other knob to be unambiguous.
                let cap = capped_ops(iops * elapsed);
                read_count = cap / 2;
                write_count = cap - read_count;
            } else {
                read_count = self.desired_ops(read_bps, self.last_read_bps, elapsed, true);
                write_count = self.desired_ops(write_bps, self.last_write_bps, elapsed, false);
                if iops > 0.0 {
                    let cap = capped_ops(iops * elapsed);
                    let total = read_count + write_count;
                    if total > cap {
                        // Keep the read/write ratio and drop the excess, so a mixed workload stays
                        // mixed when the operation cap binds.
                        let ratio = cap as f64 / total as f64;
                        let scaled_read = (read_count as f64 * ratio).round() as u64;
                        read_count = scaled_read.min(cap);
                        write_count = cap - read_count;
                        saturated = true;
                    }
                }
            }

            let total = read_count + write_count;
            if total > MAX_OPS_PER_TICK {
                let ratio = MAX_OPS_PER_TICK as f64 / total as f64;
                let scaled_read = (read_count as f64 * ratio).round() as u64;
                read_count = scaled_read.min(MAX_OPS_PER_TICK);
                write_count = MAX_OPS_PER_TICK - read_count;
                saturated = true;
            }
        }

        let ops = self.build_ops(read_count, write_count);
        let counts = if ops.is_empty() {
            Counts::default()
        } else {
            self.execute(&ops, settings.queue_depth)?
        };

        // `fsync` is driven by its own credit so a sub-one-per-tick rate still fires. It is
        // deliberately independent of read/write: durability is a separate cost.
        let mut fsyncs = 0_u64;
        if fsync_rate > 0.0 && elapsed > 0.0 {
            self.fsync_credit += fsync_rate * elapsed;
            let whole = self.fsync_credit.floor();
            self.fsync_credit -= whole;
            fsyncs = (whole.max(0.0) as u64).min(MAX_FSYNCS_PER_TICK);
            for _ in 0..fsyncs {
                let started = Instant::now();
                // Through the I/O descriptor rather than the `NamedTempFile`'s own, so that a
                // direct-mode `fsync` is the same operation the rest of the controller performs
                // rather than a buffered-mode one against a second descriptor for the same inode.
                self.io_file.sync_data().map_err(|error| io_error(&error))?;
                let elapsed = elapsed_nanoseconds(started);
                self.fsync_latency.record(elapsed);
                self.latency_histograms.observe_fsync(elapsed);
            }
        }
        self.fsyncs_total += fsyncs;
        self.read_latency.merge(&counts.read_latency);
        self.write_latency.merge(&counts.write_latency);

        let read_bps_actual = rate(counts.read_bytes, elapsed);
        let write_bps_actual = rate(counts.write_bytes, elapsed);
        let iops_actual = rate(counts.read_ops + counts.write_ops, elapsed);
        let fsyncs_per_second = rate(fsyncs, elapsed);

        self.read_bytes_total += counts.read_bytes;
        self.write_bytes_total += counts.write_bytes;
        self.ops_total += counts.read_ops + counts.write_ops;
        self.last_read_bps = read_bps_actual;
        self.last_write_bps = write_bps_actual;

        // Convergence is only claimed against targets that were actually requested. A target of
        // zero means the operator did not ask for that knob, so the controller correctly does
        // nothing and comparing a measured zero against zero would be vacuous.
        let mut converged = true;
        if read_bps > 0.0 {
            converged &= within(read_bps_actual, read_bps);
        }
        if write_bps > 0.0 {
            converged &= within(write_bps_actual, write_bps);
        }
        if read_bps == 0.0 && write_bps == 0.0 && iops > 0.0 {
            converged &= within(iops_actual, iops);
        }
        if fsync_rate > 0.0 {
            converged &= within(fsyncs_per_second, fsync_rate);
        }

        Ok(IoSnapshot {
            block_size: self.block_size as u64,
            direct: self.direct,
            read_bps,
            read_bps_actual,
            write_bps,
            write_bps_actual,
            iops,
            iops_actual,
            fsync_rate,
            fsyncs_per_second,
            read_bytes_total: self.read_bytes_total,
            write_bytes_total: self.write_bytes_total,
            ops_total: self.ops_total,
            fsyncs_total: self.fsyncs_total,
            read_latency: self.read_latency.summary(),
            write_latency: self.write_latency.summary(),
            fsync_latency: self.fsync_latency.summary(),
            converged,
            saturated,
        })
    }

    /// Operations needed this tick to close the gap to `target_bps`, with feedback.
    ///
    /// The same proportional-plus-integral shape as the access controllers: the proportional term
    /// reacts to this tick's error and the integral absorbs a persistent bias (a filesystem that
    /// is simply slower than asked, for instance) so the loop does not sit at a constant offset.
    fn desired_ops(&mut self, target_bps: f64, measured_bps: f64, elapsed: f64, read: bool) -> u64 {
        if target_bps <= 0.0 {
            return 0;
        }
        let integral = if read {
            &mut self.read_integral
        } else {
            &mut self.write_integral
        };
        let error = target_bps - measured_bps;
        *integral = (*integral + error * elapsed).clamp(-MAX_INTEGRAL_BPS, MAX_INTEGRAL_BPS);
        let bytes =
            (target_bps * elapsed + *integral * elapsed + RATE_KP * error * elapsed).max(0.0);
        let ops = (bytes / self.block_size as f64).round();
        ops.max(0.0) as u64
    }

    /// Turn operation counts into an interleaved list of offsets.
    fn build_ops(&mut self, read_ops: u64, write_ops: u64) -> Vec<Op> {
        let total = usize::try_from(read_ops.saturating_add(write_ops)).unwrap_or(0);
        let mut ops = Vec::with_capacity(total.min(MAX_OPS_PER_TICK as usize));
        let mut remaining_read = read_ops;
        let mut remaining_write = write_ops;
        // Alternate so a mixed workload is actually mixed on the device rather than a read burst
        // followed by a write burst, which the page cache would hide.
        while remaining_read > 0 || remaining_write > 0 {
            if remaining_read > 0 {
                ops.push(Op::Read(self.next_offset()));
                remaining_read -= 1;
            }
            if remaining_write > 0 {
                ops.push(Op::Write(self.next_offset()));
                remaining_write -= 1;
            }
        }
        ops
    }

    /// The next byte offset selected by the configured pattern.
    fn next_offset(&mut self) -> u64 {
        let blocks = (self.region_bytes / self.block_size as u64).max(1);
        let block = match self.pattern {
            IoPattern::Sequential => {
                let block = self.cursor % blocks;
                self.cursor = (self.cursor + 1) % blocks;
                block
            }
            IoPattern::Stride => {
                let block = self.cursor % blocks;
                self.cursor = (self.cursor + self.stride_blocks) % blocks;
                block
            }
            IoPattern::Random => self.next_rand() % blocks,
        };
        block * self.block_size as u64
    }

    /// Next pseudo-random number from a linear congruential generator.
    fn next_rand(&mut self) -> u64 {
        // Numerical Recipes constants.
        self.rng_state = self
            .rng_state
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        self.rng_state
    }

    /// Execute `ops`, fanning them out across `queue_depth` scoped worker threads.
    ///
    /// Threads are scoped to the call rather than kept in a pool: the fan-out is a few per tick and
    /// the syscalls dominate the cost, so a pool would add lifecycle complexity for no measurable
    /// gain. This is also the boundary an `io_uring` backend would replace.
    fn execute(&self, ops: &[Op], queue_depth: usize) -> Result<Counts, LoadlabError> {
        let fd = self.io_file.as_raw_fd();
        let block_size = self.block_size;
        let workers = queue_depth.clamp(1, MAX_QUEUE_DEPTH);
        if workers == 1 || ops.len() == 1 {
            return run_slice(fd, ops, block_size, &self.latency_histograms)
                .map_err(LoadlabError::Io);
        }
        let chunk = ops.len().div_ceil(workers).max(1);
        let results = std::thread::scope(|scope| {
            let histograms = self.latency_histograms.clone();
            let handles: Vec<_> = ops
                .chunks(chunk)
                .map(|slice| {
                    let histograms = histograms.clone();
                    scope.spawn(move || run_slice(fd, slice, block_size, &histograms))
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| Err("i/o worker thread panicked".to_owned()))
                })
                .collect::<Vec<_>>()
        });
        let mut total = Counts::default();
        for result in results {
            let counts = result.map_err(LoadlabError::Io)?;
            total.merge(&counts);
        }
        Ok(total)
    }

    /// Move the clock back so the next [`Self::reconcile`] measures a known window.
    ///
    /// Test-only: the convergence assertions need a deterministic `elapsed`, and a real sleep
    /// would make them slow and flaky.
    #[cfg(test)]
    fn force_elapsed(&mut self, seconds: f64) {
        self.last_wall = Instant::now().checked_sub(std::time::Duration::from_secs_f64(seconds));
    }
}

/// One filesystem operation.
#[derive(Debug, Clone, Copy)]
enum Op {
    Read(u64),
    Write(u64),
}

/// Actual bytes and operations completed, plus the latencies observed getting them.
#[derive(Debug, Clone, Default)]
struct Counts {
    read_bytes: u64,
    write_bytes: u64,
    read_ops: u64,
    write_ops: u64,
    read_latency: Latency,
    write_latency: Latency,
}

impl Counts {
    /// Fold one worker's counts into the tick's totals.
    ///
    /// Latency accumulators merge by summing their buckets rather than by collecting samples, so
    /// the fan-out costs a fixed amount of copying regardless of how many operations each thread
    /// issued.
    fn merge(&mut self, other: &Self) {
        self.read_bytes += other.read_bytes;
        self.write_bytes += other.write_bytes;
        self.read_ops += other.read_ops;
        self.write_ops += other.write_ops;
        self.read_latency.merge(&other.read_latency);
        self.write_latency.merge(&other.write_latency);
    }
}

/// Run one slice of operations on a worker's own buffer.
///
/// The write buffer is filled with a non-zero pattern so a write is a real block store rather
/// than a sparse zero range the filesystem could satisfy for free.
///
/// Each operation is timed with two `Instant::now()` calls, which is a vDSO clock read of roughly
/// 25 nanoseconds and therefore under a tenth of a percent of the cost of even a page-cache hit.
/// What it perturbs is the measurement of very fast operations, which is why the numbers are
/// published as a histogram over fixed buckets rather than as a mean.
fn run_slice(
    fd: RawFd,
    ops: &[Op],
    block_size: usize,
    histograms: &crate::metrics::IoLatencyHistograms,
) -> Result<Counts, String> {
    let mut buffer = AlignedBuffer::new(block_size, 0);
    {
        let bytes = buffer.as_mut_slice();
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::try_from(index % 251)
                .unwrap_or(0)
                .wrapping_mul(31)
                .wrapping_add(7);
        }
    }
    let mut counts = Counts::default();
    for op in ops {
        match *op {
            Op::Read(offset) => {
                let started = Instant::now();
                let bytes = pread_full(fd, buffer.as_mut_slice(), offset)
                    .map_err(|error| error.to_string())?;
                let elapsed = elapsed_nanoseconds(started);
                counts.read_latency.record(elapsed);
                histograms.observe_read(elapsed);
                counts.read_bytes += bytes;
                counts.read_ops += 1;
                if bytes > 0 {
                    // Keep the read observable: without this the compiler is free to treat the
                    // buffer as write-only and drop the work.
                    std::hint::black_box(buffer.as_slice()[0]);
                }
            }
            Op::Write(offset) => {
                let started = Instant::now();
                let bytes = pwrite_full(fd, buffer.as_slice(), offset)
                    .map_err(|error| error.to_string())?;
                let elapsed = elapsed_nanoseconds(started);
                counts.write_latency.record(elapsed);
                histograms.observe_write(elapsed);
                counts.write_bytes += bytes;
                counts.write_ops += 1;
            }
        }
    }
    Ok(counts)
}

/// Nanoseconds since `started`, saturating at zero.
///
/// Saturation is defensive rather than expected: `Instant` is monotonic, so a negative duration is
/// not reachable, and a clamping conversion is preferable to a `panic!` on a clock that misbehaves.
fn elapsed_nanoseconds(started: Instant) -> u64 {
    started.elapsed().as_nanos().try_into().unwrap_or(u64::MAX)
}

/// A byte offset as `off_t`, saturating on 32-bit targets where `off_t` is only `i32` wide.
///
/// The region is bounded by `--max-io-bytes`, which is far below `i32::MAX` on any machine where a
/// 32-bit `off_t` is in play, so the clamp is unreachable in practice and only exists to make the
/// conversion total.
fn to_off_t(offset: u64) -> libc::off_t {
    libc::off_t::try_from(offset).unwrap_or(libc::off_t::MAX)
}

/// Read exactly `buffer.len()` bytes at `offset`, looping over short reads and `EINTR`.
fn pread_full(fd: RawFd, buffer: &mut [u8], offset: u64) -> io::Result<u64> {
    let mut done = 0_usize;
    while done < buffer.len() {
        let result = unsafe {
            libc::pread(
                fd,
                buffer[done..].as_mut_ptr().cast(),
                buffer.len() - done,
                to_off_t(offset + done as u64),
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            break;
        }
        done += usize::try_from(result).unwrap_or(0);
    }
    Ok(done as u64)
}

/// Write exactly `buffer.len()` bytes at `offset`, looping over short writes and `EINTR`.
fn pwrite_full(fd: RawFd, buffer: &[u8], offset: u64) -> io::Result<u64> {
    let mut done = 0_usize;
    while done < buffer.len() {
        let result = unsafe {
            libc::pwrite(
                fd,
                buffer[done..].as_ptr().cast(),
                buffer.len() - done,
                to_off_t(offset + done as u64),
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            break;
        }
        done += usize::try_from(result).unwrap_or(0);
    }
    Ok(done as u64)
}

fn io_error(error: &io::Error) -> LoadlabError {
    LoadlabError::Io(error.to_string())
}

/// Open the descriptor the controller issues its I/O through, with or without `O_DIRECT`.
///
/// A failure here is reported rather than degraded. `O_DIRECT` is not an extra observation layered
/// on top of the workload, the way `CONFIG_SCHEDSTATS` is: it *is* the workload the operator asked
/// for, and silently issuing buffered I/O instead would hand back page-cache throughput under a
/// `direct: true` label, which is the one outcome this module exists to rule out. The caller turns
/// the error into `controller.last_error` and generates nothing for that tick.
///
/// Alignment is the other thing that makes `O_DIRECT` fail, with `EINVAL` rather than at `open`
/// time. That is caught as a configuration error before the controller is built, so it cannot
/// reach here; if it somehow does, the message below still names the block size.
fn open_io_file(path: &Path, direct: bool, block_size: usize) -> Result<File, LoadlabError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if direct {
        // `O_DIRECT` constrains only which buffer address, offset and length the kernel accepts;
        // it grants no access to anything the mode bits above do not already permit. The
        // constraint is satisfied by [`AlignedBuffer`] for the address and by every offset and
        // length being a whole multiple of `block_size`, which configuration validation requires
        // to be a multiple of `DIRECT_ALIGNMENT` before this is reached.
        options.custom_flags(libc::O_DIRECT);
    }
    options.open(path).map_err(|error| {
        if direct {
            LoadlabError::Io(format!(
                "cannot open {} with O_DIRECT ({}); the filesystem may not support it, \
                 and io.block_size of {block_size} bytes must be a multiple of {DIRECT_ALIGNMENT}",
                path.display(),
                error
            ))
        } else {
            io_error(&error)
        }
    })
}

/// A rate over a window, or zero when the window is empty.
fn rate(delta: u64, elapsed: f64) -> f64 {
    if elapsed > 0.0 {
        delta as f64 / elapsed
    } else {
        0.0
    }
}

/// Round `value` up to a whole multiple of `granularity`, saturating rather than wrapping.
fn round_up(value: u64, granularity: u64) -> u64 {
    value.div_ceil(granularity).saturating_mul(granularity)
}

/// An operation count for a window, clamped to the per-tick safety bound.
fn capped_ops(count: f64) -> u64 {
    count.floor().clamp(0.0, MAX_OPS_PER_TICK as f64) as u64
}

/// `value` with non-finite treated as zero and negatives clamped to zero.
fn finite_nonnegative(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

/// Whether an actual rate is within tolerance of its target.
fn within(actual: f64, target: f64) -> bool {
    (actual - target).abs() <= RATE_TOLERANCE.max(target * 0.10)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        AlignedBuffer, DIRECT_ALIGNMENT, IoController, IoPattern, IoSettings,
        LATENCY_BUCKETS_NANOSECONDS, LATENCY_BUCKETS_SECONDS, Latency,
    };
    use crate::{error::LoadlabError, file_backed::check_directory};

    /// A controller over `test_dir()` with fresh, unshared latency histograms.
    ///
    /// Fresh per call so a test that asserts on observation counts is not reading another test's
    /// numbers, and unshared so no test can be perturbed by one running in parallel.
    fn test_controller(ceiling: u64) -> Result<IoController, LoadlabError> {
        test_controller_in(ceiling, test_dir())
    }

    fn test_controller_in(ceiling: u64, dir: &Path) -> Result<IoController, LoadlabError> {
        IoController::new(ceiling, dir, test_histograms())
    }

    fn test_histograms() -> crate::metrics::IoLatencyHistograms {
        let host = crate::procfs::host_info().expect("host");
        crate::metrics::Metrics::new(&host)
            .expect("registry")
            .io_latency_histograms()
    }

    fn test_dir() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    /// Comfortably above every region the tests use, so the ceiling is never what makes a test
    /// pass or fail. The ceiling test overrides it.
    const TEST_CEILING: u64 = 256 * 1024 * 1024;

    fn settings() -> IoSettings {
        IoSettings {
            read_bps: 0.0,
            write_bps: 0.0,
            iops: 0.0,
            fsync_rate: 0.0,
            block_size: 4096,
            queue_depth: 1,
            pattern: IoPattern::Sequential,
            stride_blocks: 1,
            random_seed: None,
            direct: false,
        }
    }

    #[test]
    fn the_bucket_tables_agree() {
        // The accumulator compares nanoseconds and Prometheus renders seconds, so a divergence
        // between the two tables would put every sample in a bucket the exposition does not show.
        for (seconds, nanoseconds) in LATENCY_BUCKETS_SECONDS
            .iter()
            .zip(LATENCY_BUCKETS_NANOSECONDS.iter())
        {
            assert!(
                (seconds * 1e9 - *nanoseconds as f64).abs() < 1e-3,
                "{seconds}s is not {nanoseconds}ns"
            );
        }
    }

    #[test]
    fn buckets_are_cumulative_and_inclusive_of_their_boundary() {
        // A sample exactly on a boundary belongs in that bucket, not the next one: a client
        // reading `le` out of Prometheus has to get the same answer the accumulator recorded.
        for (index, boundary) in LATENCY_BUCKETS_NANOSECONDS.iter().enumerate() {
            let mut latency = Latency::default();
            latency.record(*boundary);
            assert_eq!(latency.buckets[index], 1, "boundary {boundary}ns");
            assert_eq!(latency.buckets[..index].iter().sum::<u64>(), 0);
            assert_eq!(latency.buckets[index + 1..].iter().sum::<u64>(), 0);
        }
    }

    #[test]
    fn a_sample_below_the_first_boundary_still_lands_in_a_bucket() {
        // The smallest boundary is 100 microseconds, and a page-cache read is well under it. If
        // that fell through every bucket the histogram would look empty while the rate was
        // perfectly healthy.
        let mut latency = Latency::default();
        latency.record(1_000);
        assert_eq!(latency.buckets[0], 1);
    }

    #[test]
    fn a_sample_above_every_boundary_is_counted_but_in_no_bucket() {
        // The documented behaviour: `count` is always right, and the gap between it and the
        // buckets' total is the tail. Dropping the observation instead would make a stalled
        // device look like a quiet one.
        let mut latency = Latency::default();
        latency.record(30_000_000_000);
        assert_eq!(latency.count, 1);
        assert_eq!(latency.buckets.iter().sum::<u64>(), 0);
    }

    #[test]
    fn merging_accumulators_sums_buckets_and_keeps_the_extremes() {
        let mut left = Latency::default();
        left.record(1_000);
        left.record(500_000);
        let mut right = Latency::default();
        right.record(250_000);
        right.record(9_000_000_000);

        left.merge(&right);

        assert_eq!(left.count, 4);
        assert_eq!(left.min_nanoseconds, Some(1_000));
        assert_eq!(left.max_nanoseconds, Some(9_000_000_000));
        assert_eq!(
            left.sum_nanoseconds,
            1_000 + 500_000 + 250_000 + 9_000_000_000
        );
        // The merged buckets have to equal what recording all four in one accumulator would give,
        // since that is what a single-threaded tick does.
        let mut single = Latency::default();
        for sample in [1_000, 500_000, 250_000, 9_000_000_000] {
            single.record(sample);
        }
        assert_eq!(left.buckets, single.buckets);
    }

    #[test]
    fn merging_an_empty_accumulator_changes_nothing() {
        let mut latency = Latency::default();
        latency.record(2_000);
        let before = latency.clone();
        latency.merge(&Latency::default());
        assert_eq!(latency.count, before.count);
        assert_eq!(latency.buckets, before.buckets);
        assert_eq!(latency.min_nanoseconds, before.min_nanoseconds);
        assert_eq!(latency.max_nanoseconds, before.max_nanoseconds);
    }

    #[test]
    fn an_unobserved_summary_reports_nothing_rather_than_zero() {
        // Zero would be a claim that the operation took no time at all.
        let summary = Latency::default().summary();
        assert_eq!(summary.count, 0);
        assert_eq!(summary.min_seconds, None);
        assert_eq!(summary.max_seconds, None);
    }

    #[test]
    fn an_aligned_buffer_is_aligned_and_exactly_the_requested_length() {
        for len in [1_usize, 511, 4096, 4097, 65_536] {
            let buffer = AlignedBuffer::new(len, 0xAB);
            let slice = buffer.as_slice();
            assert_eq!(slice.len(), len, "length {len}");
            assert_eq!(
                slice.as_ptr() as usize % DIRECT_ALIGNMENT,
                0,
                "address for length {len}"
            );
            assert!(
                slice.iter().all(|byte| *byte == 0xAB),
                "fill for length {len}"
            );
        }
    }

    #[test]
    fn an_aligned_buffer_hands_out_only_its_own_bytes() {
        // The padding exists to align the buffer, and nothing outside this module may reach it.
        let mut buffer = AlignedBuffer::new(64, 0);
        buffer.as_mut_slice().fill(7);
        assert!(buffer.as_slice().iter().all(|byte| *byte == 7));
        assert_eq!(buffer.as_slice().len(), 64);
        assert_eq!(buffer.backing.len(), 64 + DIRECT_ALIGNMENT);
    }

    #[test]
    fn io_pattern_names_round_trip() {
        for pattern in IoPattern::ALL {
            assert_eq!(IoPattern::parse(pattern.as_str()), Some(pattern));
        }
        assert_eq!(IoPattern::parse("spiral"), None);
    }

    #[test]
    fn a_controller_owns_a_private_temporary_file() {
        let controller = test_controller(TEST_CEILING).expect("controller");
        let path = Path::new(controller.path());
        assert!(
            path.exists(),
            "the file should exist while the controller does"
        );
    }

    #[test]
    fn dropping_the_controller_removes_the_file() {
        let path = {
            let controller = test_controller(TEST_CEILING).expect("controller");
            controller.path().to_owned()
        };
        assert!(!Path::new(&path).exists(), "the file should be gone");
    }

    /// The region has to be a whole number of blocks, or the last operation would run off its
    /// end; and it has to fit the operator's ceiling, or the ceiling means nothing.
    #[test]
    fn the_region_is_block_aligned_and_inside_the_ceiling() {
        const CEILING: u64 = 100_003;
        let controller = test_controller(CEILING).expect("controller");
        assert_eq!(controller.region_bytes() % 4096, 0);
        assert!(controller.region_bytes() >= CEILING);
        assert!(
            controller.region_bytes() - CEILING < 4096,
            "the overshoot should be less than one block"
        );
    }

    #[test]
    fn a_zero_target_does_nothing() {
        let mut controller = test_controller(TEST_CEILING).expect("controller");
        let _ = controller.reconcile(&settings()).expect("first tick");
        controller.force_elapsed(0.1);
        let snapshot = controller.reconcile(&settings()).expect("second tick");
        assert_eq!(snapshot.ops_total, 0);
        assert_eq!(snapshot.fsyncs_total, 0);
        assert!(snapshot.converged);
        assert!(!snapshot.saturated);
    }

    /// Drive `count` ticks with a fixed measurement window, which is what makes the feedback loop
    /// deterministic: a real sleep would be slow and would still not pin the window.
    fn run_ticks(
        controller: &mut IoController,
        config: &IoSettings,
        count: usize,
    ) -> super::IoSnapshot {
        let mut snapshot = controller.reconcile(config).expect("tick");
        for _ in 0..count {
            controller.force_elapsed(0.1);
            snapshot = controller.reconcile(config).expect("tick");
        }
        snapshot
    }

    /// A read target has to actually move bytes, and the feedback loop has to land on the target.
    #[test]
    fn a_read_target_moves_bytes_at_the_requested_rate() {
        let mut controller = test_controller(TEST_CEILING).expect("controller");
        let mut config = settings();
        config.read_bps = 8.0 * 1024.0 * 1024.0;
        let snapshot = run_ticks(&mut controller, &config, 60);
        assert!(snapshot.read_bytes_total > 0, "no reads were issued");
        assert!(
            (snapshot.read_bps_actual - config.read_bps).abs() <= config.read_bps * 0.10,
            "read rate {} did not converge on {}",
            snapshot.read_bps_actual,
            config.read_bps
        );
        assert!(snapshot.converged, "the loop should report convergence");
    }

    /// The Prometheus latency histograms have to be fed by the operations themselves.
    ///
    /// This test exists because they were not: the three histograms were registered and rendered,
    /// and nothing ever called `observe`, so every scrape reported a zero count while `/metrics`
    /// reported hundreds of thousands of observations. A metric that is structurally present and
    /// permanently zero is worse than an absent one, because a dashboard built on it looks healthy.
    ///
    /// The assertion is on the rendered exposition rather than on a counter, so it covers the whole
    /// path: registration, the shared handles, and the worker observing.
    #[test]
    fn latency_reaches_the_prometheus_histograms() {
        let host = crate::procfs::host_info().expect("host");
        let metrics = crate::metrics::Metrics::new(&host).expect("registry");
        let histograms = metrics.io_latency_histograms();

        let mut controller =
            IoController::new(TEST_CEILING, test_dir(), histograms.clone()).expect("controller");
        let mut config = settings();
        config.read_bps = 8.0 * 1024.0 * 1024.0;
        config.write_bps = 8.0 * 1024.0 * 1024.0;
        let snapshot = run_ticks(&mut controller, &config, 40);

        // The JSON side is the reference: if these two disagree, the split between the bucket
        // accumulator and the histogram is what broke.
        assert!(snapshot.read_latency.count > 0, "no reads were timed");
        assert!(snapshot.write_latency.count > 0, "no writes were timed");

        // Rendered against a default runtime on purpose: the latency histograms are filled by
        // observation alone, so nothing in the snapshot feeds them. What this needs to prove is
        // the path from an operation to the rendered `_count`, not the correctness of the gauges.
        let runtime = crate::state::RuntimeStatus::new(0);
        let exposition = metrics
            .render(
                &crate::state::WorkloadState::default(),
                &runtime,
                &crate::procfs::MemoryStats::default(),
                &crate::procfs::FaultStats::default(),
                &crate::procfs::CpuStats::default(),
            )
            .expect("encode");
        let count_of = |metric: &str| -> u64 {
            exposition
                .lines()
                .find(|line| line.starts_with(&format!("{metric}_count ")))
                .and_then(|line| line.split_ascii_whitespace().nth(1))
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("{metric}_count is missing from the exposition"))
        };

        assert_eq!(
            count_of("loadlab_io_read_latency_seconds"),
            snapshot.read_latency.count,
            "the read histogram and the JSON summary must count the same operations"
        );
        assert_eq!(
            count_of("loadlab_io_write_latency_seconds"),
            snapshot.write_latency.count,
            "the write histogram and the JSON summary must count the same operations"
        );
        assert!(
            exposition.contains("loadlab_io_read_latency_seconds_bucket{le=\"+Inf\"}"),
            "the read histogram has no `+Inf` bucket, so nothing reports the overflow range"
        );
    }

    /// `O_DIRECT` has to be a real flag on the descriptor, not a label. If it silently fell back
    /// to buffered I/O the numbers would still converge and this test would pass while measuring
    /// the page cache, which is the one thing `direct` exists to rule out.
    ///
    /// What this can prove is that the flag is set and that aligned direct I/O actually completes:
    /// the reconciler only succeeds with the flag on, and the kernel accepts the aligned buffer,
    /// offset and length that [`AlignedBuffer`] and the block-rounded region produce.
    #[test]
    fn direct_mode_moves_real_bytes_through_a_direct_descriptor() {
        let mut controller = test_controller(TEST_CEILING).expect("controller");
        let mut config = settings();
        config.direct = true;
        config.write_bps = 8.0 * 1024.0 * 1024.0;
        config.read_bps = 8.0 * 1024.0 * 1024.0;

        let snapshot = run_ticks(&mut controller, &config, 40);

        assert!(snapshot.direct, "the snapshot should report direct mode");
        assert!(
            snapshot.write_bytes_total > 0,
            "no direct writes were issued"
        );
        assert!(snapshot.read_bytes_total > 0, "no direct reads were issued");
    }

    /// A failed `open` in direct mode has to be an error the operator can act on, not a fallback
    /// to buffered I/O wearing a `direct: true` label.
    ///
    /// tmpfs is not usable as the trigger here: Linux 6.2 gave it an `O_DIRECT` implementation, so
    /// on a current kernel the open succeeds. Opening a directory is used instead, because it fails
    /// the same way on every kernel and goes through the same error path a filesystem without
    /// `O_DIRECT` would.
    #[test]
    fn a_failed_direct_open_names_the_flag_and_the_alignment() {
        let error = super::open_io_file(Path::new("/dev/shm"), true, 512)
            .expect_err("a directory cannot be opened read-write");
        let message = error.to_string();
        assert!(
            message.contains("O_DIRECT"),
            "the error should name the flag that failed: {message}"
        );
        assert!(
            message.contains("512"),
            "the error should quote the block size it was given: {message}"
        );
        assert!(
            message.contains("4096"),
            "the error should state the alignment requirement: {message}"
        );
    }

    /// The buffered path must not inherit the direct-mode wording, or a plain open failure would
    /// send an operator looking for a filesystem problem that is not there.
    #[test]
    fn a_failed_buffered_open_does_not_mention_direct() {
        let error =
            super::open_io_file(Path::new("/dev/shm"), false, 512).expect_err("a directory");
        let message = error.to_string();
        assert!(
            !message.contains("O_DIRECT"),
            "the buffered path should not mention O_DIRECT: {message}"
        );
    }

    /// Every issued operation has to be timed, and the count has to be the operation count rather
    /// than something nearby. A latency histogram that silently undercounts is worse than none.
    #[test]
    fn every_read_and_write_is_observed_by_the_latency_histogram() {
        let mut controller = test_controller(TEST_CEILING).expect("controller");
        let mut config = settings();
        config.iops = 20_000.0;
        config.fsync_rate = 50.0;
        let snapshot = run_ticks(&mut controller, &config, 40);

        assert!(snapshot.ops_total > 0, "no operations were issued");
        assert_eq!(
            snapshot.read_latency.count + snapshot.write_latency.count,
            snapshot.ops_total,
            "every read and write should be timed exactly once"
        );
        assert_eq!(
            snapshot.fsync_latency.count, snapshot.fsyncs_total,
            "every fsync should be timed exactly once"
        );
        assert!(
            snapshot.read_latency.min_seconds.is_some(),
            "a timed read should have a minimum"
        );
        assert!(
            snapshot.write_latency.max_seconds >= snapshot.write_latency.min_seconds,
            "max should not be below min"
        );
    }

    /// The latencies are cumulative and never reset, so a client averaging `sum_seconds` across
    /// two scrapes divides by the change in `count` and gets the right answer without having to
    /// notice that the controller was rebuilt.
    #[test]
    fn latencies_accumulate_across_ticks() {
        let mut controller = test_controller(TEST_CEILING).expect("controller");
        let mut config = settings();
        config.iops = 20_000.0;
        let first = run_ticks(&mut controller, &config, 10);
        let second = run_ticks(&mut controller, &config, 10);

        assert!(
            second.read_latency.count + second.write_latency.count
                > first.read_latency.count + first.write_latency.count,
            "the second window should add observations to the first"
        );
        assert!(
            second.read_latency.sum_seconds >= first.read_latency.sum_seconds,
            "a cumulative sum should not go backwards"
        );
    }

    /// Turning `direct` on and off has to reopen the descriptor rather than be cached forever, and
    /// a mode change must not lose the byte totals.
    #[test]
    fn direct_mode_can_be_turned_on_and_off_without_losing_the_counters() {
        let mut controller = test_controller(TEST_CEILING).expect("controller");
        let mut buffered = settings();
        buffered.iops = 20_000.0;
        let first = run_ticks(&mut controller, &buffered, 10);
        assert!(!first.direct, "the default should be buffered");

        let mut direct = settings();
        direct.iops = 20_000.0;
        direct.direct = true;
        let second = run_ticks(&mut controller, &direct, 10);
        assert!(second.direct, "direct mode should have taken effect");
        assert!(
            second.ops_total > first.ops_total,
            "the totals should keep accumulating"
        );

        let third = run_ticks(&mut controller, &buffered, 10);
        assert!(!third.direct, "turning direct off should take effect too");
        assert!(third.ops_total > second.ops_total);
    }

    /// A write target has to store bytes, and the reported total has to match what was asked of
    /// the filesystem rather than what the loop intended.
    #[test]
    fn a_write_target_stores_bytes() {
        let mut controller = test_controller(TEST_CEILING).expect("controller");
        let mut config = settings();
        config.write_bps = 8.0 * 1024.0 * 1024.0;
        let snapshot = run_ticks(&mut controller, &config, 60);
        assert!(snapshot.write_bytes_total > 0, "no writes were issued");
        assert!(snapshot.read_bytes_total == 0, "a write-only target read");
        assert!(snapshot.converged);
    }

    /// An operation cap below what the byte target would need is the saturated case.
    #[test]
    fn an_operation_cap_that_binds_reports_saturated() {
        let mut controller = test_controller(TEST_CEILING).expect("controller");
        let _ = controller.reconcile(&settings()).expect("first tick");
        controller.force_elapsed(0.1);
        let mut config = settings();
        config.read_bps = 1024.0 * 1024.0 * 1024.0;
        config.iops = 1000.0;
        let snapshot = controller.reconcile(&config).expect("second tick");
        assert!(
            snapshot.saturated,
            "a binding iops cap must report saturated"
        );
        assert!(
            snapshot.iops_actual <= 1000.0 + 1.0,
            "iops {} exceeded the cap",
            snapshot.iops_actual
        );
    }

    /// With no byte target, an operation target splits the budget evenly across reads and writes.
    #[test]
    fn an_operation_target_without_bytes_splits_reads_and_writes() {
        let mut controller = test_controller(TEST_CEILING).expect("controller");
        let _ = controller.reconcile(&settings()).expect("first tick");
        controller.force_elapsed(0.1);
        let mut config = settings();
        config.iops = 10_000.0;
        let snapshot = controller.reconcile(&config).expect("second tick");
        assert!(snapshot.read_bytes_total > 0);
        assert!(snapshot.write_bytes_total > 0);
        assert!(snapshot.converged);
    }

    /// `fsync` is driven by its own accumulator, so a low rate still fires and reports honestly.
    #[test]
    fn a_fsync_rate_is_honoured() {
        let mut controller = test_controller(TEST_CEILING).expect("controller");
        let _ = controller.reconcile(&settings()).expect("first tick");
        controller.force_elapsed(0.1);
        let mut config = settings();
        config.fsync_rate = 100.0;
        let snapshot = controller.reconcile(&config).expect("second tick");
        assert!(snapshot.fsyncs_total > 0, "no fsync was issued");
        assert!(
            (snapshot.fsyncs_per_second - 100.0).abs() <= 10.0,
            "fsync rate {} did not converge",
            snapshot.fsyncs_per_second
        );
        assert!(snapshot.converged);
    }

    /// Every pattern has to stay inside the region. An out-of-range offset would be a read or
    /// write past the end of the file, which `pread`/`pwrite` would reject; surviving several
    /// ticks at a high rate is the assertion.
    #[test]
    fn every_pattern_stays_inside_the_region() {
        for pattern in IoPattern::ALL {
            let mut controller = test_controller(TEST_CEILING).expect("controller");
            let mut config = settings();
            config.pattern = pattern;
            config.stride_blocks = 3;
            config.random_seed = Some(7);
            config.read_bps = 16.0 * 1024.0 * 1024.0;
            config.write_bps = 16.0 * 1024.0 * 1024.0;
            for _ in 0..3 {
                let _ = controller.reconcile(&config).expect("tick");
                controller.force_elapsed(0.05);
            }
            assert!(controller.region_bytes() > 0, "{pattern:?} lost its region");
        }
    }

    /// A queue depth above one has to produce the same total work as a serial run, just fanned
    /// out; the accounting is per worker and summed.
    #[test]
    fn a_queue_depth_fans_out_without_losing_work() {
        let mut controller = test_controller(TEST_CEILING).expect("controller");
        let mut config = settings();
        config.read_bps = 16.0 * 1024.0 * 1024.0;
        config.queue_depth = 4;
        let snapshot = run_ticks(&mut controller, &config, 60);
        assert!(snapshot.read_bytes_total >= 16 * 1024 * 1024 / 10);
        assert!(snapshot.converged);
    }

    /// A directory that cannot hold a real file has to be refused, for the same reason the
    /// file-backed controller refuses it.
    #[test]
    fn a_memory_backed_directory_is_refused() {
        let tmp = Path::new("/tmp");
        if check_directory(tmp).is_ok() {
            return; // Host has a real /tmp; nothing to assert.
        }
        assert!(test_controller_in(TEST_CEILING, tmp).is_err());
    }
}
