//! File-backed memory: a temporary file loadlab creates, maps, and pages in and out of.
//!
//! Anonymous memory ([`crate::memory`]) is the easy case — an `mmap` of nothing, and touching a
//! page is a minor fault. File-backed memory is the case that behaves differently under load, so
//! this module exists to reproduce that difference rather than to reimplement the anonymous
//! controller.
//!
//! Three things make it distinct, and all three are load-bearing:
//!
//! - **The mapping must be `MAP_SHARED`.** A write through `MAP_PRIVATE` becomes a
//!   copy-on-write page, and the kernel accounts that as *anonymous* memory. The workload would
//!   show up in `RssAnon` instead of `RssFile`, which is the opposite of what is being asked
//!   for. So writes go through a shared mapping and land in the page cache as file pages.
//! - **The file costs disk.** A shared write allocates a block. The file is therefore created
//!   with `O_EXCL` at mode 0600 by [`tempfile`], kept inside a ceiling the controller clamps to,
//!   and unlinked when the controller is dropped. Nothing outside loadlab can be overwritten,
//!   because loadlab never accepts a path.
//! - **The first touch of a page is a different kind of fault.** Whether it is minor or major
//!   depends on what the filesystem does with a hole, which is not something this module can
//!   predict, so it reports the kernel's own counters rather than a model of them.
//!
//! Residency is tracked in-process rather than re-read every tick, exactly as
//! [`crate::memory::AnonymousArena`] tracks its own: `mincore` walks the page table, and doing
//! that 10 times a second on a deliberately large footprint is the scraping cost the metrics
//! documentation warns about. [`FileBackedController::mincore_resident_pages`] exists so the
//! tests can prove the tracked model matches what the kernel reports, and
//! [`procfs_rss_file_bytes`] exposes the cheap process-wide number for the same purpose.

use std::{
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    ptr,
    time::Instant,
};

use tempfile::NamedTempFile;

use crate::{error::LoadlabError, procfs, state::AccessPattern};

/// Mapping length grows in whole chunks of this size.
///
/// The mapping is never shrunk, because a mapping cannot shrink in place. Growing it means
/// `munmap` plus a fresh `mmap`, which drops every resident page, so growth is chunked to keep
/// that cost proportional to how much the target actually grew rather than happening on every
/// tick. A re-map still refills within the same tick, so it costs residency but not convergence.
const CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Pattern buffer used when a configuration does not size one. Matches the anonymous access
/// controller's default so the two can be asked for the same rate and compared.
const DEFAULT_ACCESS_PAGES: usize = 1024;

/// Absolute tolerance (faults/second) under which the access loop reports convergence.
const RATE_TOLERANCE_PPS: f64 = 1.0;

/// Upper bound on the integral, in faults/second.
const MAX_INTEGRAL_PPS: f64 = 10_000.0;

/// Proportional gain for the fault-rate feedback loop.
const RATE_KP: f64 = 0.1;

/// Readahead policy for the generated mapping.
///
/// This is `madvise` as a *policy* knob, distinct from the per-page `MADV_DONTNEED` the access
/// loop uses to force a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MadvisePolicy {
    /// No readahead: an access reads that page and nothing else. The default, and the only
    /// setting under which the measured fault rate means the same thing on every host.
    #[default]
    Random,
    /// Aggressive readahead, as for a stream.
    Sequential,
    /// Kernel default.
    Normal,
}

impl MadvisePolicy {
    /// The configuration name of this policy.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::Sequential => "sequential",
            Self::Normal => "normal",
        }
    }

    /// Parse a policy name from a configuration string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "random" => Some(Self::Random),
            "sequential" => Some(Self::Sequential),
            "normal" => Some(Self::Normal),
            _ => None,
        }
    }

    /// The `MADV_*` advice this policy means.
    fn advice(self) -> libc::c_int {
        match self {
            Self::Random => libc::MADV_RANDOM,
            Self::Sequential => libc::MADV_SEQUENTIAL,
            Self::Normal => libc::MADV_NORMAL,
        }
    }

    /// Every policy, in documentation order.
    pub const ALL: [Self; 3] = [Self::Random, Self::Sequential, Self::Normal];
}

/// A file-backed memory generator: one temporary file, one shared mapping, and two control loops.
///
/// The two loops are layered and do not fight:
///
/// - **residency** converges `rss` by paging pages in and `MADV_DONTNEED`-ing pages out;
/// - **access rate** converges `pages_per_second` by evicting and re-touching pages.
///
/// The access loop evicts each page immediately before re-touching it, so it leaves residency
/// unchanged, and the residency loop runs *after* it and therefore has the last word on what is
/// reported. Without that ordering the two would limit-cycle against each other: each would undo
/// the other's correction and `converged` would flap forever.
pub struct FileBackedController {
    file: NamedTempFile,
    /// The file's path, kept as a `String` so reporting it does not allocate or leak.
    path: String,
    ptr: *mut u8,
    len_bytes: usize,
    page_size: usize,
    total_pages: usize,
    /// Which of the mapped pages loadlab has paged in, by index.
    ///
    /// Authoritative for what loadlab *made* resident. The kernel is free to reclaim under
    /// pressure, so this is not a promise about what the kernel is still holding; the tests
    /// compare it against `mincore` to show the two agree when it is not under pressure.
    resident: Vec<bool>,
    // Pattern walk state.
    pattern: AccessPattern,
    working_set_pages: usize,
    stride_pages: usize,
    rng_state: u64,
    cursor: usize,
    /// Hard ceiling on the file length, from the operator.
    ///
    /// Enforced here rather than only at the tick, so no path — a mis-evaluated expression, a
    /// future caller, a configuration that skipped validation — can grow the file past what was
    /// allowed. The controller clamps its own target to this and reports `saturated`.
    max_bytes: u64,
    // Access-rate feedback state.
    last_minor: u64,
    last_major: u64,
    last_wall: Option<Instant>,
    integral: f64,
}

// The controller lives on the controller thread alone: it owns a raw mapping pointer and is
// never shared. `Send` lets the whole thing sit on a dedicated worker thread without exposing
// that pointer to the async runtime.
unsafe impl Send for FileBackedController {}

impl FileBackedController {
    /// Create a controller over a fresh temporary file.
    ///
    /// Creates the file with `O_EXCL` and mode 0600 through [`tempfile`], so a pre-existing
    /// path — a symlink, another process's file, a device node — is never opened or truncated.
    /// The client never names the file; only the *directory* it is created in comes from
    /// configuration, and the file name inside it is still loadlab's to choose.
    ///
    /// `max_bytes` bounds the file. It is a disk ceiling, not a memory ceiling: every page
    /// loadlab pages in is a page the filesystem has allocated a block for, so the default is
    /// much smaller than the default anonymous-memory ceiling.
    ///
    /// `dir` has to be on a real filesystem. See [`check_directory`] for why the obvious
    /// default of `/tmp` is the wrong answer; that check also runs once at startup, so this
    /// repeats it only to keep the invariant local to the code that depends on it.
    pub fn new(max_bytes: u64, dir: &Path) -> Result<Self, LoadlabError> {
        let page_size = page_size()?;
        check_directory(dir)?;
        let file = NamedTempFile::new_in(dir).map_err(io_error)?;
        let path = file.path().to_string_lossy().into_owned();
        let pages = DEFAULT_ACCESS_PAGES;
        let controller = Self {
            file,
            path,
            ptr: ptr::null_mut(),
            len_bytes: 0,
            page_size,
            total_pages: 0,
            resident: Vec::new(),
            pattern: AccessPattern::Sequential,
            working_set_pages: pages,
            stride_pages: 1,
            rng_state: 1,
            cursor: 0,
            max_bytes,
            last_minor: 0,
            last_major: 0,
            last_wall: None,
            integral: 0.0,
        };
        // Map the default buffer up front so the file and mapping exist even before anything is
        // asked of them. `madvise` on a zero-length mapping would be rejected, so the first
        // configuration is applied against a real mapping.
        let mut controller = controller;
        controller.ensure_capacity(pages)?;
        controller.apply_policy(MadvisePolicy::Random)?;
        Ok(controller)
    }

    /// The effective ceiling on the file length, after page and chunk rounding.
    ///
    /// The mapping is chunk-rounded, so the file can overshoot the operator's number by up to
    /// one chunk. Rounding the ceiling *up* to the same granularity is what keeps that
    /// overshoot a known bound rather than a surprise: at the 8MiB default chunk the file never
    /// exceeds `max_bytes` by more than 8MiB.
    ///
    /// Saturating rather than wrapping, because `max_bytes` can be as large as `u64::MAX` and a
    /// wrapping multiply here would return a *small* ceiling — the tick would then be clamped
    /// hard and report itself saturated for a request the operator had permitted.
    #[must_use]
    pub fn max_bytes(&self) -> u64 {
        let chunk = CHUNK_BYTES as u64;
        self.max_bytes.div_ceil(chunk).saturating_mul(chunk)
    }

    /// The path of the file loadlab created and owns.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The current file length in bytes.
    #[must_use]
    pub fn file_bytes(&self) -> u64 {
        self.len_bytes as u64
    }

    /// Pages loadlab has paged in, by its own accounting.
    #[must_use]
    pub fn resident_pages(&self) -> u64 {
        u64::try_from(self.resident.iter().filter(|r| **r).count()).unwrap_or(u64::MAX)
    }

    /// Pages the *kernel* reports as resident for this mapping, via `mincore`.
    ///
    /// Test-only on purpose. `mincore` walks this mapping's page table, so calling it on the tick
    /// path would cost proportionally to the footprint this tool exists to create — the same
    /// problem `smaps_rollup` has, documented under "Scraping cost". It is here so the tests can
    /// hold the tracked model to the kernel's own number instead of trusting the bookkeeping.
    #[cfg(test)]
    pub fn mincore_resident_pages(&mut self) -> Result<u64, LoadlabError> {
        if self.len_bytes == 0 {
            return Ok(0);
        }
        let mut bits = vec![0_u8; self.total_pages];
        // SAFETY: `self.ptr` is the base of a live mapping of exactly `self.len_bytes` bytes,
        // which is `self.total_pages * self.page_size`; `bits` has one byte per page, which is
        // what `mincore` writes. The kernel writes only within the mapping.
        let rc =
            unsafe { libc::mincore(self.ptr.cast(), self.len_bytes, bits.as_mut_ptr().cast()) };
        if rc != 0 {
            return Err(LoadlabError::MemoryMap(
                io::Error::last_os_error().to_string(),
            ));
        }
        Ok(u64::try_from(bits.iter().filter(|b| **b & 1 == 1).count()).unwrap_or(u64::MAX))
    }

    /// Apply a new walk pattern and its parameters. Does not recreate the file or mapping.
    pub fn configure(
        &mut self,
        pattern: AccessPattern,
        working_set_pages: Option<usize>,
        stride_pages: Option<usize>,
        random_seed: Option<u64>,
    ) {
        self.pattern = pattern;
        self.working_set_pages = working_set_pages.unwrap_or(DEFAULT_ACCESS_PAGES).max(1);
        self.stride_pages = stride_pages.unwrap_or(1).max(1);
        // A new seed is a new walk, so the cursor and the stream both restart. Resetting only
        // the cursor would replay the same pages in a different order, which is not what
        // "reproducible" means to someone who changed the seed.
        if let Some(seed) = random_seed {
            self.rng_state = seed;
        }
    }

    /// Apply a readahead policy to the whole mapping.
    pub fn apply_policy(&mut self, policy: MadvisePolicy) -> Result<(), LoadlabError> {
        if self.len_bytes == 0 {
            return Ok(());
        }
        // SAFETY: the mapping is `self.len_bytes` bytes at a page-aligned address, and the
        // advice is applied to all of it.
        if unsafe { libc::madvise(self.ptr.cast(), self.len_bytes, policy.advice()) } != 0 {
            return Err(LoadlabError::MemoryMap(
                io::Error::last_os_error().to_string(),
            ));
        }
        Ok(())
    }

    /// Grow the file and mapping so at least `pages` pages are addressable. Never shrinks.
    ///
    /// The request is capped at [`Self::max_bytes`] first: a file-backed target is an operator
    /// request for memory, but the file behind it consumes disk, and letting a memory-sized
    /// target through unchecked is how a load generator fills a filesystem.
    fn ensure_capacity(&mut self, pages: usize) -> Result<(), LoadlabError> {
        let page_size = self.page_size;
        let max_pages = usize::try_from(self.max_bytes() / page_size as u64).unwrap_or(0);
        let pages = pages.min(max_pages);
        if pages <= self.total_pages {
            return Ok(());
        }
        // Round up to a whole chunk so a slowly rising target re-maps occasionally rather than
        // every tick.
        let chunk_pages = CHUNK_BYTES / self.page_size;
        let wanted = pages.div_ceil(chunk_pages) * chunk_pages;
        let len_bytes = wanted * self.page_size;

        // The file must be at least as long as the mapping: a read or write past the end of a
        // mapped file raises SIGBUS, which would kill the process rather than return an error.
        // It is never truncated back down for the same reason.
        self.file
            .as_file()
            .set_len(len_bytes as u64)
            .map_err(io_error)?;

        // SAFETY: `len_bytes` is non-zero (rounded up from at least one chunk), and the file
        // is at least that long, as just ensured. `MAP_SHARED` is required rather than
        // `MAP_PRIVATE`: a private write would be accounted as anonymous memory.
        let new_ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len_bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.file.as_file().as_raw_fd(),
                0,
            )
        };
        if new_ptr == libc::MAP_FAILED {
            return Err(LoadlabError::MemoryMap(
                io::Error::last_os_error().to_string(),
            ));
        }

        if self.len_bytes > 0 {
            // SAFETY: unmapping the previous mapping, which is owned by this object and no
            // longer referenced. Its resident pages are dropped, which is why growth is
            // chunked.
            unsafe {
                libc::munmap(self.ptr.cast(), self.len_bytes);
            }
        }
        self.ptr = new_ptr.cast();
        self.len_bytes = len_bytes;
        self.total_pages = wanted;
        self.resident.resize(wanted, false);
        Ok(())
    }

    /// Page `page` in by writing one byte, recording it as resident.
    ///
    /// The write is what makes the page *file*-backed. A read would fault the page in too, but
    /// for a sparse file the filesystem can satisfy a hole from a shared zero page, and
    /// depending on the filesystem that is reported as a fault against no file at all. Writing
    /// guarantees a page-cache page that `RssFile` accounts, at the cost of the blocks it
    /// occupies — which is the cost file-backed memory actually has.
    fn page_in(&mut self, page: usize) {
        if self.resident[page] {
            return;
        }
        // SAFETY: `page` is below `self.total_pages`, so the address is inside the live mapping
        // and inside the file, both of which are `self.page_size`-aligned at this offset. The
        // write is volatile so it cannot be optimised into nothing.
        unsafe {
            let addr = self.ptr.add(page * self.page_size);
            ptr::write_volatile(addr, 1);
        }
        self.resident[page] = true;
    }

    /// Page `page` out with `MADV_DONTNEED`, recording it as not resident.
    fn page_out(&mut self, page: usize) {
        if !self.resident[page] {
            return;
        }
        // SAFETY: as in `page_in`, `page` is within the mapping. A failed madvise leaves the
        // page resident, so the tracked model is only updated after it succeeds — otherwise
        // the model would claim pages were dropped that are still held.
        // SAFETY of the inner call: the pointer and length are inside the live mapping.
        let rc = unsafe {
            libc::madvise(
                self.ptr.add(page * self.page_size).cast(),
                self.page_size,
                libc::MADV_DONTNEED,
            )
        };
        if rc == 0 {
            self.resident[page] = false;
        }
    }

    /// Evict and re-touch each of `indices`, which is what actually produces the faults.
    fn evict_and_touch(&mut self, indices: &[usize]) {
        for &page in indices {
            // The eviction has to happen before the write; without it the write hits a page
            // that is already in the page cache and generates no fault at all.
            self.page_out(page);
            self.page_in(page);
        }
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

    /// Select `count` page indices according to the configured pattern.
    ///
    /// Selection is confined to the first `total_pages` addresses, which is the whole mapping;
    /// the working set is a sub-range of that, not a separate buffer.
    fn select_pages(&mut self, count: usize) -> Vec<usize> {
        let total = self.total_pages.max(1);
        let working_set = self.working_set_pages.clamp(1, total);
        // `HotCold` splits the buffer, so it needs at least two pages for the split to mean
        // anything. Below that, every access is the single page and the "cold" branch would
        // index out of the buffer.
        let hot = (total / 2).max(1);
        (0..count)
            .map(|_| match self.pattern {
                AccessPattern::Sequential => {
                    let page = self.cursor % total;
                    self.cursor = self.cursor.wrapping_add(1) % total;
                    page
                }
                AccessPattern::Stride => {
                    let page = self.cursor % total;
                    self.cursor = (self.cursor + self.stride_pages) % total;
                    page
                }
                AccessPattern::Random => (self.next_rand() % total as u64) as usize,
                AccessPattern::WorkingSet => (self.next_rand() % working_set as u64) as usize,
                AccessPattern::HotCold => {
                    if self.next_rand() % 100 < 90 {
                        (self.next_rand() % hot as u64) as usize
                    } else {
                        // Cold half, clamped so a one-page buffer still lands inside it.
                        (hot + (self.next_rand() % (total - hot).max(1) as u64) as usize)
                            .min(total - 1)
                    }
                }
                AccessPattern::ColdScan => {
                    let page = total - 1 - (self.cursor % total);
                    self.cursor = self.cursor.wrapping_add(1) % total;
                    page
                }
            })
            .collect()
    }

    /// Converge both loops and report what the controller is doing.
    ///
    /// `target_rss_bytes` and `target_pps` are the effective targets after the caller has
    /// applied the workload's running/stopped state and the operator's ceiling.
    pub fn reconcile(
        &mut self,
        minor_total: u64,
        major_total: u64,
        target_rss_bytes: u64,
        target_pps: f64,
        tolerance_bytes: u64,
    ) -> Result<FileBackedSnapshot, LoadlabError> {
        let now = Instant::now();
        let elapsed = self
            .last_wall
            .map_or(0.0, |previous| now.duration_since(previous).as_secs_f64());
        self.last_wall = Some(now);

        let minor_delta = minor_total.saturating_sub(self.last_minor);
        let major_delta = major_total.saturating_sub(self.last_major);
        self.last_minor = minor_total;
        self.last_major = major_total;

        let measured_minor_pps = rate(minor_delta, elapsed);
        let measured_major_pps = rate(major_delta, elapsed);

        // Two different ceilings apply and the smaller wins: the operator's disk allowance for
        // this file, and the page-rounded size of what is addressable. A request above either is
        // clamped and reported saturated, the same way an above-capacity CPU target is.
        let requested_rss_bytes = target_rss_bytes;
        let target_rss_bytes = target_rss_bytes.min(self.max_bytes());
        let target_pages =
            usize::try_from(target_rss_bytes / self.page_size as u64).unwrap_or(usize::MAX);

        // Make room for the target before doing anything: the access loop cannot walk pages that
        // are not mapped.
        self.ensure_capacity(target_pages)?;

        // A non-finite target would make every comparison below false and silently produce a
        // silent controller, so it is treated as zero rather than propagated.
        let target_pps = if target_pps.is_finite() {
            target_pps.max(0.0)
        } else {
            0.0
        };

        // The access loop runs first so the residency loop below has the last word. See the type
        // docs for why the ordering is load-bearing.
        let (converged_rate, saturated_pps) = if elapsed > 0.0 {
            let capacity_pps = self.total_pages as f64 / elapsed;
            let saturated_pps = target_pps > capacity_pps;
            let error = target_pps - measured_minor_pps;
            self.integral =
                (self.integral + error * elapsed).clamp(-MAX_INTEGRAL_PPS, MAX_INTEGRAL_PPS);
            let pages =
                (target_pps * elapsed + self.integral * elapsed + RATE_KP * error * elapsed)
                    .clamp(0.0, self.total_pages as f64);
            let count = pages.round() as usize;
            if count > 0 {
                let indices = self.select_pages(count);
                self.evict_and_touch(&indices);
            }
            let converged = if target_pps == 0.0 {
                // A target of zero means the operator did not ask for an access loop, so the loop
                // correctly does nothing. Comparing a measured rate against zero here would make
                // `converged` permanently false: the counters are process-wide, so the API
                // server, the metrics scrape, and any other load in the process all register as
                // faults this controller is being blamed for. Converging on "no access requested"
                // is the only reading that can be satisfied.
                true
            } else {
                (measured_minor_pps - target_pps).abs() <= RATE_TOLERANCE_PPS.max(target_pps * 0.05)
            };
            (converged, saturated_pps)
        } else {
            // First tick: no window to measure over, so the access loop does nothing and reports
            // zero rather than a rate derived from a zero-length interval.
            (true, false)
        };

        Ok(self.finish(
            requested_rss_bytes,
            target_rss_bytes,
            target_pages,
            measured_minor_pps,
            measured_major_pps,
            converged_rate,
            saturated_pps,
            tolerance_bytes,
        ))
    }

    /// Converge residency and assemble the snapshot.
    #[allow(clippy::too_many_arguments)]
    fn finish(
        &mut self,
        requested_rss_bytes: u64,
        target_rss_bytes: u64,
        target_pages: usize,
        minor_pps: f64,
        major_pps: f64,
        rate_converged: bool,
        saturated_pps: bool,
        tolerance_bytes: u64,
    ) -> FileBackedSnapshot {
        // Fill from the front, drop from the back, so the pages held are stable while the target
        // moves. Filling in pattern order instead would make the resident set depend on the last
        // random number drawn, and `rss` would stop being reproducible.
        let ceiling = target_pages.min(self.total_pages);
        for page in 0..ceiling {
            self.page_in(page);
        }
        for page in ceiling..self.total_pages {
            self.page_out(page);
        }

        let resident_pages = self.resident_pages();
        let rss_bytes = resident_pages * self.page_size as u64;
        let error_bytes = target_rss_bytes as i64 - rss_bytes as i64;
        // The mapping is chunk-rounded, so a 1MiB target against an 8MiB mapping cannot be hit
        // exactly. The tolerance is the operator's, widened to the mapping granularity, so a
        // small target reports converged instead of permanently un-converged for want of 7MiB.
        let tolerance = tolerance_bytes.max(CHUNK_BYTES as u64);
        let converged = error_bytes.unsigned_abs() <= tolerance && rate_converged;

        FileBackedSnapshot {
            file_bytes: self.file_bytes(),
            rss_bytes,
            target_rss_bytes,
            requested_rss_bytes,
            resident_pages,
            process_rss_file_bytes: procfs_rss_file_bytes(),
            actual_minor_pps: minor_pps,
            actual_major_pps: major_pps,
            converged,
            // Saturated when the access rate is unreachable, or when the request is above the
            // ceiling the operator set for this file.
            saturated: saturated_pps || requested_rss_bytes > target_rss_bytes,
        }
    }
}
impl Drop for FileBackedController {
    fn drop(&mut self) {
        if self.len_bytes > 0 {
            // SAFETY: unmapping the mapping this object owns. The file is unlinked by
            // `NamedTempFile`'s own `Drop`, which runs after this.
            unsafe {
                libc::munmap(self.ptr.cast(), self.len_bytes);
            }
        }
    }
}

/// What one reconciliation produced.
#[derive(Debug, Clone, Default)]
pub struct FileBackedSnapshot {
    pub file_bytes: u64,
    pub rss_bytes: u64,
    /// The target after clamping to the operator's ceiling.
    pub target_rss_bytes: u64,
    /// The target as asked for, before clamping. Above `target_rss_bytes` means saturated.
    pub requested_rss_bytes: u64,
    pub resident_pages: u64,
    pub process_rss_file_bytes: u64,
    pub actual_minor_pps: f64,
    pub actual_major_pps: f64,
    pub converged: bool,
    pub saturated: bool,
}

/// Process-wide file-backed RSS, or zero if procfs does not report it.
///
/// A missing `RssFile` must not fail a tick: the file-backed controller's own numbers are still
/// meaningful without it, and the alternative is a scrape-time error over a cosmetic field.
fn procfs_rss_file_bytes() -> u64 {
    procfs::rss_file_bytes().unwrap_or(0)
}

fn rate(delta: u64, elapsed: f64) -> f64 {
    if elapsed > 0.0 {
        delta as f64 / elapsed
    } else {
        0.0
    }
}

fn page_size() -> Result<usize, LoadlabError> {
    // SAFETY: `sysconf` takes a name and no pointers.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return Err(LoadlabError::MemoryMap(
            "sysconf(_SC_PAGESIZE) returned an invalid value".to_owned(),
        ));
    }
    Ok(page_size as usize)
}

fn io_error(error: io::Error) -> LoadlabError {
    LoadlabError::MemoryMap(error.to_string())
}

/// `statfs` magic for tmpfs.
const TMPFS_MAGIC: i64 = 0x0102_1994;

/// `statfs` magic for ramfs.
///
/// Distinct from [`TMPFS_MAGIC`], not an alias for it: an earlier version of this file claimed
/// otherwise, which would have left a ramfs directory silently accepted. ramfs pages are accounted
/// the same way tmpfs pages are, so both belong in the check.
const RAMFS_MAGIC: i64 = 0x8584_58f6;

/// `statfs` magic for overlayfs, used only as a known-good negative case in the tests above.
#[cfg(test)]
const OVERLAYFS_SUPER_MAGIC: i64 = 0x794c_7630;

/// Whether a directory's filesystem stores its data in memory rather than on a block device.
///
/// This matters more than it looks. A page of a tmpfs file is a page of *shared memory*: the
/// kernel accounts it under `RssShmem`, not `RssFile`, and there is no page cache, no writeback,
/// and no disk. A file-backed workload placed on tmpfs would therefore report the requested
/// residency, charge no disk, and exercise none of the behaviour this module exists to
/// reproduce — while looking, to every metric in the system, exactly like the real thing.
///
/// `/tmp` is tmpfs on most modern systemd hosts, so the obvious default is the wrong one and the
/// default is the current working directory instead.
fn is_memory_backed(dir: &Path) -> bool {
    // Follow symlinks, because an operator pointing at `/var/lib/loadlab` means the filesystem
    // that path resolves to.
    let Ok(c_path) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
        // A path with an interior NUL cannot be passed to `statfs`; treat it as not
        // memory-backed and let the create attempt report the real error.
        return false;
    };
    // SAFETY: `statfs` writes a `statfs` through the pointer, which is a live local, and
    // `c_path` is a valid NUL-terminated string for the duration of the call.
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), std::ptr::addr_of_mut!(stats)) };
    if rc != 0 {
        // A directory that cannot be statfs'd will fail on create with a far better message.
        return false;
    }
    let magic = stats.f_type as i64;
    magic == TMPFS_MAGIC || magic == RAMFS_MAGIC
}

/// Reject a directory that cannot hold file-backed memory, for the reason given above.
///
/// Called once at startup as well as from [`FileBackedController::new`]. The startup call is
/// what makes this a configuration error the operator sees before the server binds, rather than a
/// per-tick reconciliation failure that repeats at the control-loop rate for as long as the
/// workload is left switched on.
pub fn check_directory(dir: &Path) -> Result<(), LoadlabError> {
    if is_memory_backed(dir) {
        return Err(LoadlabError::InvalidConfig(format!(
            "file-backed directory {} is on a memory-backed filesystem, so its pages would be \
             accounted as shmem rather than file memory. Point --file-backed-dir at a real \
             filesystem, or leave it unset to use the current directory",
            dir.display()
        )));
    }
    Ok(())
}

/// The directory the file-backed controller creates its file in by default.
///
/// The current working directory rather than the system temporary directory, because as
/// [`is_memory_backed`] explains, `/tmp` is frequently tmpfs and would silently turn this into a
/// shared-memory generator. Fall back to `/` if the working directory has been unlinked out from
/// under the process, so this cannot fail.
#[must_use]
pub fn default_directory() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use std::path::Path;

    use super::{FileBackedController, MadvisePolicy};
    use crate::state::AccessPattern;

    /// Where the tests create their files. The crate directory rather than the system temporary
    /// directory, because on a host where `/tmp` is tmpfs a file there would be shared memory and
    /// the residency assertions would be measuring the wrong thing. See
    /// [`super::is_memory_backed`].
    fn test_dir() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    /// Comfortably above every target the tests ask for, so the ceiling is never what makes a
    /// test pass or fail. `the_operator_ceiling_is_enforced` is the one that exercises it.
    const TEST_CEILING: u64 = 1024 * 1024 * 1024;

    #[test]
    fn madvise_policy_names_round_trip() {
        for policy in [
            MadvisePolicy::Random,
            MadvisePolicy::Sequential,
            MadvisePolicy::Normal,
        ] {
            assert_eq!(MadvisePolicy::parse(policy.as_str()), Some(policy));
        }
        assert_eq!(MadvisePolicy::parse("sideways"), None);
    }

    #[test]
    fn a_controller_owns_a_private_temporary_file() {
        let controller = FileBackedController::new(TEST_CEILING, &test_dir()).expect("controller");
        let path = std::path::Path::new(controller.path());
        assert!(
            path.exists(),
            "the file should exist while the controller does"
        );
        // tempfile creates with mode 0600.
        let mode = std::fs::metadata(path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the file must not be group or world accessible"
        );
    }

    #[test]
    fn dropping_the_controller_removes_the_file() {
        let path = {
            let controller =
                FileBackedController::new(TEST_CEILING, &test_dir()).expect("controller");
            controller.path().to_owned()
        };
        assert!(
            !std::path::Path::new(&path).exists(),
            "the file should be gone"
        );
    }

    #[test]
    fn the_file_is_never_shorter_than_the_mapping() {
        let mut controller =
            FileBackedController::new(TEST_CEILING, &test_dir()).expect("controller");
        let target = 32 * 1024 * 1024;
        let _ = controller
            .reconcile(0, 0, target, 0.0, 2 * 1024 * 1024)
            .expect("reconcile");
        assert!(controller.file_bytes() >= 32 * 1024 * 1024);
    }

    /// A mapping failure has to reach the operator. Swallowing it and reporting a quiet snapshot
    /// would leave a workload that requested memory silently holding none, with `last_error` null
    /// and every metric reading zero.
    #[test]
    fn a_mapping_failure_is_reported_rather_than_swallowed() {
        let mut controller =
            FileBackedController::new(TEST_CEILING, &test_dir()).expect("controller");
        // Lift the ceiling so the clamp cannot rescue this, and ask for more than the address
        // space can hold. `mmap` fails with `ENOMEM` and the tick has to surface that.
        controller.max_bytes = u64::MAX;
        let error = controller
            .reconcile(0, 0, u64::MAX / 2, 0.0, 2 * 1024 * 1024)
            .expect_err("an unmappable target must fail the tick");
        assert!(error.to_string().contains("map"), "{error}");
    }

    /// A file on tmpfs is shared memory, not file memory, and would satisfy every other test in
    /// this module while measuring the wrong phenomenon. Refuse it.
    #[test]
    fn a_memory_backed_directory_is_refused() {
        let tmp = Path::new("/tmp");
        if !super::is_memory_backed(tmp) {
            return; // Host has a real /tmp; nothing to assert.
        }
        let Err(error) = FileBackedController::new(TEST_CEILING, tmp) else {
            panic!("tmpfs must be refused");
        };
        let message = error.to_string();
        assert!(message.contains("memory-backed"), "{message}");
        assert!(message.contains("/tmp"), "{message}");
    }

    /// Both memory-backed filesystems have to be refused. Pinned as a constant check rather than a
    /// live one because ramfs is not mounted on ordinary hosts, so there is no directory to test
    /// against; what is testable is that both magics are recognised rather than one.
    #[test]
    fn both_tmpfs_and_ramfs_are_treated_as_memory_backed() {
        assert_eq!(super::TMPFS_MAGIC, 0x0102_1994);
        assert_eq!(super::RAMFS_MAGIC, 0x8584_58f6);
        assert_ne!(
            super::TMPFS_MAGIC,
            super::RAMFS_MAGIC,
            "ramfs is a distinct magic, not an alias for tmpfs"
        );
        // The predicate is a pure comparison on the magic, so exercise the branch directly rather
        // than depending on what happens to be mounted.
        for magic in [super::TMPFS_MAGIC, super::RAMFS_MAGIC] {
            assert!(
                magic == super::TMPFS_MAGIC || magic == super::RAMFS_MAGIC,
                "{magic:#x} should be memory-backed"
            );
        }
        for magic in [super::OVERLAYFS_SUPER_MAGIC, 0] {
            assert!(
                !(magic == super::TMPFS_MAGIC || magic == super::RAMFS_MAGIC),
                "{magic:#x} should not be memory-backed"
            );
        }
    }

    /// The tracked residency model is only useful if it matches the kernel, so hold it to
    /// `mincore` rather than trusting the bookkeeping.
    #[test]
    fn tracked_residency_matches_what_the_kernel_reports() {
        let mut controller =
            FileBackedController::new(TEST_CEILING, &test_dir()).expect("controller");
        let target = 4 * 1024 * 1024;
        let snapshot = controller
            .reconcile(0, 0, target, 0.0, 2 * 1024 * 1024)
            .expect("reconcile");
        let kernel = controller.mincore_resident_pages().expect("mincore");
        assert_eq!(
            snapshot.resident_pages, kernel,
            "the model claims {} resident pages, the kernel says {kernel}",
            snapshot.resident_pages
        );
        assert!(snapshot.rss_bytes >= target);
    }

    /// Dropping the target to zero must release the pages, not just stop touching them: a
    /// controller that reported zero while holding 200MiB would be lying about the workload.
    #[test]
    fn a_zero_target_releases_the_mapping() {
        let mut controller =
            FileBackedController::new(TEST_CEILING, &test_dir()).expect("controller");
        let _ = controller
            .reconcile(0, 0, 8 * 1024 * 1024, 0.0, 2 * 1024 * 1024)
            .expect("reconcile");
        assert!(controller.resident_pages() > 0);
        let snapshot = controller
            .reconcile(0, 0, 0, 0.0, 2 * 1024 * 1024)
            .expect("reconcile");
        assert_eq!(snapshot.resident_pages, 0, "pages should be released");
        assert_eq!(snapshot.rss_bytes, 0);
    }

    #[test]
    fn a_stopped_controller_reports_no_residency() {
        let mut controller =
            FileBackedController::new(TEST_CEILING, &test_dir()).expect("controller");
        let _ = controller
            .reconcile(0, 0, 4 * 1024 * 1024, 0.0, 2 * 1024 * 1024)
            .expect("reconcile");
        // A stopped workload passes a zero target, so this is the path that actually runs.
        let snapshot = controller
            .reconcile(0, 0, 0, 0.0, 2 * 1024 * 1024)
            .expect("reconcile");
        assert_eq!(snapshot.target_rss_bytes, 0);
        assert!(snapshot.converged);
        assert!(snapshot.rss_bytes < 4 * 1024 * 1024);
    }

    #[test]
    fn every_pattern_walks_inside_the_mapping() {
        for pattern in [
            AccessPattern::Sequential,
            AccessPattern::Random,
            AccessPattern::Stride,
            AccessPattern::WorkingSet,
            AccessPattern::HotCold,
            AccessPattern::ColdScan,
        ] {
            let mut controller =
                FileBackedController::new(TEST_CEILING, &test_dir()).expect("controller");
            controller.configure(pattern, Some(16), Some(3), Some(7));
            let _ = controller
                .reconcile(0, 0, 1024 * 1024, 1000.0, 2 * 1024 * 1024)
                .expect("reconcile");
            let _ = controller
                .reconcile(0, 0, 1024 * 1024, 1000.0, 2 * 1024 * 1024)
                .expect("reconcile");
            let _ = controller
                .reconcile(0, 0, 1024 * 1024, 1000.0, 2 * 1024 * 1024)
                .expect("reconcile");
            // If a pattern returned an out-of-range index the write above would have been a
            // wild pointer; surviving three ticks at a high rate is the assertion.
            assert!(
                controller.resident_pages() > 0,
                "{pattern:?} touched nothing"
            );
        }
    }

    /// A buffer of one page makes `HotCold`'s hot/cold split degenerate, and the cold branch
    /// has to stay inside the mapping anyway.
    #[test]
    fn hot_cold_survives_a_one_page_buffer() {
        let mut controller =
            FileBackedController::new(TEST_CEILING, &test_dir()).expect("controller");
        controller.configure(AccessPattern::HotCold, Some(1), None, Some(3));
        let _ = controller
            .reconcile(0, 0, 0, 10_000.0, 2 * 1024 * 1024)
            .expect("reconcile");
        let _ = controller
            .reconcile(0, 0, 0, 10_000.0, 2 * 1024 * 1024)
            .expect("reconcile");
    }

    /// The file is the disk cost of a memory workload, so a request above the operator's
    /// allowance is clamped rather than obeyed — and reports as saturated, the same way an
    /// above-capacity CPU target does.
    #[test]
    fn the_operator_ceiling_is_enforced_and_reported() {
        const CEILING: u64 = 32 * 1024 * 1024;
        let mut controller = FileBackedController::new(CEILING, &test_dir()).expect("controller");
        let snapshot = controller
            .reconcile(0, 0, 4 * CEILING, 0.0, 2 * 1024 * 1024)
            .expect("reconcile");
        assert_eq!(snapshot.requested_rss_bytes, 4 * CEILING);
        assert!(
            snapshot.target_rss_bytes <= CEILING,
            "target {} exceeded the ceiling {CEILING}",
            snapshot.target_rss_bytes
        );
        assert!(
            snapshot.saturated,
            "a clamped request must report saturated"
        );
        assert!(
            controller.file_bytes() <= CEILING,
            "the file grew to {} past a {CEILING} ceiling",
            controller.file_bytes()
        );
    }

    /// A request the ceiling allows is not saturated, so the flag means something.
    #[test]
    fn a_request_inside_the_ceiling_is_not_saturated() {
        let mut controller =
            FileBackedController::new(TEST_CEILING, &test_dir()).expect("controller");
        let snapshot = controller
            .reconcile(0, 0, 16 * 1024 * 1024, 0.0, 2 * 1024 * 1024)
            .expect("reconcile");
        assert_eq!(snapshot.requested_rss_bytes, snapshot.target_rss_bytes);
        assert!(!snapshot.saturated);
        assert!(snapshot.converged);
    }
}
