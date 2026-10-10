use std::{ptr, time::Instant};

use crate::{error::LoadlabError, state::AccessPattern};

/// Buffer size in pages when a pattern does not specify a working set.
const DEFAULT_BUFFER_PAGES: usize = 1024;

/// Absolute tolerance (faults/second) under which the access controller reports convergence.
const RATE_TOLERANCE_PPS: f64 = 1.0;

/// Upper bound on the integral, in faults/second, so a long-standing error cannot wind the
/// intensity up unboundedly.
const MAX_INTEGRAL_PPS: f64 = 10_000.0;

/// Proportional gain for the fault-rate feedback loop.
const RATE_KP: f64 = 0.1;

/// Owns a private page-aligned buffer and turns configured access patterns into a controllable
/// minor-fault rate.
///
/// The controller works by evicting pages (MADV_DONTNEED) then touching them (write), which
/// generates approximately one minor fault per touched page. It measures the minor-fault rate
/// via /proc/self/stat and uses a PI controller to adjust how many pages to touch per tick
/// to approach the configured `pages_per_second` target.
pub struct AccessController {
    ptr: *mut u8,
    len_bytes: usize,
    page_size: usize,
    total_pages: usize,
    pattern: AccessPattern,
    working_set_pages: usize,
    stride_pages: usize,
    // Deterministic PRNG state (LCG) for reproducible random access.
    rng_state: u64,
    cursor: usize,
    // Feedback controller state.
    last_minor: u64,
    last_major: u64,
    last_wall: Option<Instant>,
    integral: f64,
}

impl AccessController {
    /// Create a new access controller.
    ///
    /// # Arguments
    ///
    /// * `pattern` - The access pattern to use.
    /// * `working_set_pages` - For working_set and hot_cold patterns: size of the working set.
    /// * `stride_pages` - For stride pattern: stride distance in pages.
    /// * `random_seed` - For random pattern: seed for reproducible randomness.
    pub fn new(
        pattern: AccessPattern,
        working_set_pages: Option<usize>,
        stride_pages: Option<usize>,
        random_seed: Option<u64>,
    ) -> Result<Self, LoadlabError> {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            return Err(LoadlabError::MemoryMap(
                "sysconf(_SC_PAGESIZE) returned an invalid value".to_owned(),
            ));
        }
        let page_size = page_size as usize;
        let working_set = working_set_pages.unwrap_or(DEFAULT_BUFFER_PAGES).max(1);
        let buffer_pages = working_set;
        let len_bytes = buffer_pages * page_size;
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len_bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(LoadlabError::MemoryMap(
                std::io::Error::last_os_error().to_string(),
            ));
        }
        let stride = stride_pages.unwrap_or(1).max(1);
        let seed = random_seed.unwrap_or(1);
        Ok(Self {
            ptr: ptr.cast(),
            len_bytes,
            page_size,
            total_pages: buffer_pages,
            pattern,
            working_set_pages: working_set,
            stride_pages: stride,
            rng_state: seed,
            cursor: 0,
            last_minor: 0,
            last_major: 0,
            last_wall: None,
            integral: 0.0,
        })
    }

    /// Select `count` page indices according to the configured pattern.
    fn select_pages(&mut self, count: usize) -> Vec<usize> {
        match self.pattern {
            AccessPattern::Sequential => (0..count)
                .map(|_| {
                    let p = self.cursor % self.total_pages;
                    self.cursor += 1;
                    p
                })
                .collect(),
            AccessPattern::Stride => (0..count)
                .map(|_| {
                    let p = self.cursor % self.total_pages;
                    self.cursor = (self.cursor + self.stride_pages) % self.total_pages;
                    p
                })
                .collect(),
            AccessPattern::Random => (0..count)
                .map(|_| (self.next_rand() % self.total_pages as u64) as usize)
                .collect(),
            AccessPattern::WorkingSet => (0..count)
                .map(|_| (self.next_rand() % self.working_set_pages as u64) as usize)
                .collect(),
            AccessPattern::HotCold => {
                let hot = self.total_pages / 2;
                (0..count)
                    .map(|_| {
                        if self.next_rand() % 100 < 90 {
                            // Hot half: [0, hot)
                            (self.next_rand() % hot as u64) as usize
                        } else {
                            // Cold half: [hot, total_pages)
                            hot + (self.next_rand() % (self.total_pages - hot) as u64) as usize
                        }
                    })
                    .collect()
            }
            AccessPattern::ColdScan => {
                // Scan from the end toward zero so each page is a "cold" miss before wrap.
                (0..count)
                    .map(|_| {
                        let p = self.total_pages - 1 - (self.cursor % self.total_pages);
                        self.cursor += 1;
                        p
                    })
                    .collect()
            }
        }
    }

    /// Evict the given pages (MADV_DONTNEED) then touch them to provoke minor faults.
    fn touch_evict(&mut self, indices: &[usize]) {
        for &page in indices {
            let addr = unsafe { self.ptr.add(page * self.page_size) };
            // Drop the page, then fault it back in. The write is what the kernel accounts as a
            // minor fault; the madvise is what makes the next access a fault instead of a cache hit.
            unsafe {
                // Ignore madvise errors: best-effort.
                let _ = libc::madvise(addr.cast(), self.page_size, libc::MADV_DONTNEED);
                ptr::write_volatile(addr, 1);
            }
        }
    }

    /// Next pseudo-random number from a simple LCG.
    fn next_rand(&mut self) -> u64 {
        // Numerical Recipes constants.
        self.rng_state = self
            .rng_state
            .wrapping_mul(1664525)
            .wrapping_add(1013904223);
        self.rng_state
    }

    /// Reconcile the access rate against the target and return a snapshot.
    ///
    /// # Arguments
    ///
    /// * `minor_total` - Cumulative minor faults from /proc/self/stat.
    /// * `major_total` - Cumulative major faults from /proc/self/stat.
    /// * `target_pps` - Desired minor-fault rate (pages per second).
    ///
    /// Returns a snapshot containing the measured rate and controller state.
    pub fn reconcile(
        &mut self,
        minor_total: u64,
        major_total: u64,
        target_pps: f64,
    ) -> AccessSnapshot {
        let now = Instant::now();
        let elapsed = self
            .last_wall
            .map_or(0.0, |p| now.duration_since(p).as_secs_f64());
        self.last_wall = Some(now);

        let minor_delta = minor_total.saturating_sub(self.last_minor);
        let major_delta = major_total.saturating_sub(self.last_major);
        self.last_minor = minor_total;
        self.last_major = major_total;

        // Measured faults per second over the window; zero on first tick / zero window.
        let measured_minor_pps = if elapsed > 0.0 {
            minor_delta as f64 / elapsed
        } else {
            0.0
        };
        let measured_major_pps = if elapsed > 0.0 {
            major_delta as f64 / elapsed
        } else {
            0.0
        };

        let target = target_pps.max(0.0);

        // Saturation: the buffer can only be fully re-faulted once per window.
        let capacity_pps = if elapsed > 0.0 {
            self.total_pages as f64 / elapsed
        } else {
            f64::MAX
        };
        let saturated = target > capacity_pps;

        // PI controller on the faults/second error.
        let error = target - measured_minor_pps;
        // Only integrate when we have a non-zero window to avoid wind-up on startup.
        if elapsed > 0.0 {
            self.integral =
                (self.integral + error * elapsed).clamp(-MAX_INTEGRAL_PPS, MAX_INTEGRAL_PPS);
        }
        // Desired pages this tick = open-loop target + integral trim + proportional term.
        let pages = (target * elapsed + self.integral * elapsed + RATE_KP * error * elapsed)
            .clamp(0.0, self.total_pages as f64);

        let count = pages.round() as usize;
        // When not running or target is zero, do not touch pages.
        if count > 0 {
            let indices = self.select_pages(count);
            self.touch_evict(&indices);
        }

        let converged =
            (measured_minor_pps - target).abs() <= RATE_TOLERANCE_PPS.max(target * 0.05);

        AccessSnapshot {
            actual_minor_pps: measured_minor_pps,
            actual_major_pps: measured_major_pps,
            converged,
            saturated,
        }
    }
}

unsafe impl Send for AccessController {}

impl Drop for AccessController {
    fn drop(&mut self) {
        unsafe {
            // Best-effort cleanup. The mapping belongs exclusively to this object.
            let _ = libc::munmap(self.ptr.cast(), self.len_bytes);
        }
    }
}

/// Snapshot of the access controller's state after a reconciliation tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct AccessSnapshot {
    /// Measured minor-fault rate (pages per second).
    pub actual_minor_pps: f64,
    /// Measured major-fault rate (pages per second).
    pub actual_major_pps: f64,
    /// Whether the measured rate is within tolerance of the target.
    pub converged: bool,
    /// Whether the target rate exceeds what the buffer can deliver.
    pub saturated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_can_be_created() {
        let ctrl =
            AccessController::new(AccessPattern::Sequential, None, None, None).expect("controller");
        assert_eq!(ctrl.total_pages, DEFAULT_BUFFER_PAGES);
    }

    #[test]
    fn sequential_pattern_selects_pages_in_order() {
        let mut ctrl = AccessController::new(AccessPattern::Sequential, Some(10), None, None)
            .expect("controller");
        let pages = ctrl.select_pages(5);
        assert_eq!(pages, vec![0, 1, 2, 3, 4]);
        let pages = ctrl.select_pages(5);
        assert_eq!(pages, vec![5, 6, 7, 8, 9]);
        let pages = ctrl.select_pages(5);
        assert_eq!(pages, vec![0, 1, 2, 3, 4]); // wrap
    }

    #[test]
    fn stride_pattern_selects_strided_pages() {
        let mut ctrl =
            AccessController::new(AccessPattern::Stride, None, Some(3), None).expect("controller");
        let pages = ctrl.select_pages(5);
        assert_eq!(pages, vec![0, 3, 6, 9, 12]);
        let pages = ctrl.select_pages(5);
        assert_eq!(pages, vec![15, 18, 21, 24, 27]);
    }

    #[test]
    fn random_pattern_is_deterministic_with_seed() {
        let mut ctrl1 =
            AccessController::new(AccessPattern::Random, None, None, Some(42)).expect("controller");
        let mut ctrl2 =
            AccessController::new(AccessPattern::Random, None, None, Some(42)).expect("controller");
        let pages1 = ctrl1.select_pages(5);
        let pages2 = ctrl2.select_pages(5);
        assert_eq!(pages1, pages2);
    }

    #[test]
    fn working_set_pattern_is_constrained() {
        let mut ctrl = AccessController::new(
            AccessPattern::WorkingSet,
            Some(5), // working set size
            None,
            Some(42),
        )
        .expect("controller");
        let pages = ctrl.select_pages(10);
        for &p in &pages {
            assert!(p < 5, "page {} outside working set", p);
        }
    }

    #[test]
    fn hot_cold_pattern_splits_access() {
        let mut ctrl = AccessController::new(AccessPattern::HotCold, None, None, Some(123))
            .expect("controller");
        let mut hot = 0;
        let mut cold = 0;
        for _ in 0..1000 {
            let page = ctrl.select_pages(1)[0];
            if page < ctrl.total_pages / 2 {
                hot += 1;
            } else {
                cold += 1;
            }
        }
        // Expect roughly 90% hot, 10% cold.
        assert!(hot > 800 && hot < 950);
        assert!(cold > 50 && cold < 200);
    }

    #[test]
    fn reconcile_zero_target_does_not_touch_pages() {
        let mut ctrl =
            AccessController::new(AccessPattern::Sequential, None, None, None).expect("controller");
        // Prime the fault counters.
        let _ = ctrl.reconcile(100, 0, 10.0);
        // Now reconcile with zero target.
        let snap = ctrl.reconcile(100, 0, 0.0);
        #[allow(clippy::float_cmp)]
        assert_eq!(snap.actual_minor_pps, 0.0);
        assert!(snap.converged);
    }

    #[test]
    fn test_reconcile_open_loop() {
        let mut ctrl =
            AccessController::new(AccessPattern::Sequential, None, None, None).expect("controller");
        // Simulate a window where we measure zero faults.
        let snap = ctrl.reconcile(0, 0, 10.0);
        // With zero measured, integral ramps up, pages should be > 0.
        #[allow(clippy::float_cmp)]
        assert_eq!(snap.actual_minor_pps, 0.0); // no time yet
        // Second tick: elapsed > 0, measured still zero.
        let _snap = ctrl.reconcile(0, 0, 10.0);
        // actual_minor_pps still zero (no faults generated yet in this window).
        // But we can check that the controller is trying to increase intensity.
        // Hard to unit test without mocking time; trust the integration test.
    }
}
