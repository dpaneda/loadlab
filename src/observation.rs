//! Observations that `/proc` records per thread rather than per process.
//!
//! Everything else in loadlab observes the process as a whole: `/proc/self/stat`,
//! `smaps_rollup`, `status`. That is the right unit for memory and for CPU time, and it stays the
//! unit for those. It is the wrong unit for two questions this module answers.
//!
//! **Was a CPU worker runnable but not running?** A target of 100% on one worker that reports 60%
//! has two very different explanations: the duty cycle is wrong, or the thread spent the window
//! waiting for a CPU. `/proc/self/stat` cannot tell them apart, because a descheduled thread
//! consumes no CPU time and is therefore indistinguishable from a thread that was never asked to
//! work. Per-thread `schedstat` is the only procfs measurement that separates them.
//!
//! **Are the process's page faults the ones we generated?** The memory and file-backed controllers
//! touch pages on purpose, from the controller thread. A process-wide fault counter also counts every
//! fault the runtime, the allocator and the HTTP server take incidentally, so the number the
//! controllers report cannot be read as "generated faults" without knowing the split.
//!
//! Both answers come from reading per-thread procfs files, which is what [`crate::procfs`] is for.
//! This module holds only the interpretation: finding loadlab's own threads by name and folding
//! their counters into one figure each.
//!
//! # The one measurement procfs cannot give: reclaim
//!
//! **Reclaim events** are invisible to procfs. When the kernel takes pages back under pressure,
//! nothing in `/proc/<pid>` records it, and that is a real gap here specifically because loadlab
//! also releases pages on purpose with `MADV_DONTNEED` — from procfs the two are identical,
//! `RssAnon` falls either way. `/proc/vmstat` counts reclaim for the whole system and cannot
//! attribute any of it to loadlab.
//!
//! This is the one thing eBPF would buy, and eBPF is absent on purpose. It costs a C/clang build
//! dependency, CO-RE relocation against the running kernel's BTF, `CAP_BPF` or root, and breakage
//! across kernel versions — a price worth paying for one measurement, but not one worth paying
//! before the measurement is wanted. `clang` is an optional build requirement when it arrives: a kernel without the tooling
//! gets a loadlab that runs identically and reports this backend as unavailable.
//!
//! The two observations here are the procfs-backed half of that seam. When the eBPF backend lands it
//! adds a third observation beside them and a way to report availability per backend; it does not
//! change what these two mean, because neither of them depends on an event stream.

use crate::procfs::{self, ThreadScheduling};

/// Scheduling behaviour of the CPU worker threads, summed.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct WorkerScheduling {
    /// Whether the kernel exposes per-thread scheduler statistics at all.
    ///
    /// False on a kernel built without `CONFIG_SCHEDSTATS`. The counters below are then zero, which
    /// is *not* a measurement of zero waiting, and `available` is what tells the two apart.
    pub available: bool,
    /// Worker threads found at the time of the sample.
    pub threads: usize,
    /// Nanoseconds the workers spent on a CPU.
    pub on_cpu_nanoseconds: u64,
    /// Nanoseconds the workers spent runnable but not running.
    pub runqueue_wait_nanoseconds: u64,
    /// The largest run-queue wait of any single worker.
    ///
    /// The sum says how much waiting happened in total; this says whether it was spread evenly or
    /// one worker was starved while the others ran. A pool that reports its worst worker is
    /// describing the pool rather than its average member.
    pub worst_runqueue_wait_nanoseconds: u64,
    /// Times the workers were scheduled onto a CPU.
    pub timeslices: u64,
}

/// Page faults split between the controller thread and everything else in the process.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct FaultAttribution {
    /// The process-wide totals the split below was taken from. Carried here rather than re-read at
    /// the call site so that the halves and the whole they add up to come from one sample: reading
    /// them separately makes the arithmetic disagree by however many faults this process took in
    /// between.
    pub process_minor_faults: u64,
    /// The same, for major faults.
    pub process_major_faults: u64,
    /// Whether the controller thread's own counters could be read.
    ///
    /// False when they could not, in which case the split below is zero rather than a division that
    /// was never made.
    pub controller_found: bool,
    /// Minor faults taken by the controller thread.
    pub controller_minor_faults: u64,
    /// Major faults taken by the controller thread.
    pub controller_major_faults: u64,
    /// Minor faults taken by every other thread.
    pub other_minor_faults: u64,
    /// Major faults taken by every other thread.
    pub other_major_faults: u64,
}

/// Sum the scheduling statistics of the given CPU worker threads.
///
/// `workers` are the kernel ids the pool reported, so this reads the pool being described and
/// nothing else. The ids come from the workers themselves rather than from a scan of the
/// process's threads by name, because a thread inherits its creator's `comm` and only takes its
/// own once the creator's `pthread_setname_np` lands: a worker spawned this tick can still be
/// reading as `loadlab-controller`, which would report the pool as having no run-queue data at
/// all on the tick it was created.
///
/// A worker that exits between being listed here and having its files read is skipped rather
/// than failing the sample: the pool is rebuilt on configuration changes, and one stale entry is
/// not a reason to report no scheduling data for a tick.
#[must_use]
pub fn worker_scheduling(workers: &[u32]) -> WorkerScheduling {
    let mut scheduling = WorkerScheduling::default();
    for &tid in workers {
        let Ok(reading) = procfs::thread_scheduling(tid) else {
            continue;
        };
        accumulate(&mut scheduling, reading);
    }
    scheduling
}

fn accumulate(scheduling: &mut WorkerScheduling, reading: ThreadScheduling) {
    scheduling.available = true;
    scheduling.threads += 1;
    scheduling.on_cpu_nanoseconds = scheduling
        .on_cpu_nanoseconds
        .saturating_add(reading.on_cpu_nanoseconds);
    scheduling.runqueue_wait_nanoseconds = scheduling
        .runqueue_wait_nanoseconds
        .saturating_add(reading.runqueue_wait_nanoseconds);
    scheduling.worst_runqueue_wait_nanoseconds = scheduling
        .worst_runqueue_wait_nanoseconds
        .max(reading.runqueue_wait_nanoseconds);
    scheduling.timeslices = scheduling.timeslices.saturating_add(reading.timeslices);
}

/// Split the process's page faults into the controller thread's and the rest.
///
/// `controller_tid` is passed in rather than looked up because the caller *is* that thread: a
/// lookup would mean scanning every thread's `comm` on every tick to rediscover a fact the thread
/// already knows. The caller resolves it once with [`current_thread_id`].
pub fn fault_attribution(faults: &procfs::FaultStats, controller_tid: u32) -> FaultAttribution {
    // The controller thread is the thread that takes the faults, so it keeps faulting while this
    // function runs. No ordering of the two reads below avoids the resulting disagreement: the
    // process-wide aggregate in /proc/<pid>/stat is not sampled atomically with the per-thread
    // counters, so whichever order is chosen, the aggregate can arrive trailing the thread that is
    // faulting right now. The split therefore clamps instead of subtracting, below.
    let controller = procfs::thread_fault_stats(controller_tid).ok();

    let Some(controller) = controller else {
        // Without the controller's own counters the split cannot be made, and reporting the whole
        // process as "other" would quietly mean the opposite of what it says.
        return FaultAttribution::default();
    };

    // The two halves are apportioned from the process total rather than independently reported, so
    // that they always add up to a number an operator already trusts. Clamping the controller's share
    // to the total can cost it a fault on a tick where it is faulting faster than the aggregate is
    // refreshed; that fault lands in `other` instead. The alternative -- letting the two halves
    // disagree -- would make every rate computed from them wrong by an unbounded amount, which is a
    // far worse trade for a per-tick discrepancy of one or two faults.
    let controller_minor_faults = controller.minor_faults.min(faults.minor_faults);
    let controller_major_faults = controller.major_faults.min(faults.major_faults);

    FaultAttribution {
        process_minor_faults: faults.minor_faults,
        process_major_faults: faults.major_faults,
        controller_found: true,
        controller_minor_faults,
        controller_major_faults,
        other_minor_faults: faults.minor_faults.saturating_sub(controller_minor_faults),
        other_major_faults: faults.major_faults.saturating_sub(controller_major_faults),
    }
}

/// The calling thread's kernel id.
///
/// Used once, at controller startup, to identify the thread whose page faults are loadlab's own.
pub fn current_thread_id() -> u32 {
    // SAFETY: `gettid` takes no arguments, reads no memory and cannot fail. It returns the calling
    // thread's own kernel id, which is positive, so the conversion only rejects the impossible case.
    let tid = unsafe { libc::gettid() };
    u32::try_from(tid).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{WorkerScheduling, accumulate};
    use crate::procfs::ThreadScheduling;

    #[test]
    fn a_worker_reading_is_summed_and_the_worst_is_kept() {
        let mut scheduling = WorkerScheduling::default();
        accumulate(
            &mut scheduling,
            ThreadScheduling {
                tid: 1,
                on_cpu_nanoseconds: 100,
                runqueue_wait_nanoseconds: 10,
                timeslices: 2,
            },
        );
        accumulate(
            &mut scheduling,
            ThreadScheduling {
                tid: 2,
                on_cpu_nanoseconds: 50,
                runqueue_wait_nanoseconds: 900,
                timeslices: 1,
            },
        );

        assert!(
            scheduling.available,
            "a reading proves the kernel has the counters"
        );
        assert_eq!(scheduling.threads, 2);
        assert_eq!(scheduling.on_cpu_nanoseconds, 150);
        assert_eq!(scheduling.runqueue_wait_nanoseconds, 910);
        // The worst, not the mean: the starved worker is the interesting one.
        assert_eq!(scheduling.worst_runqueue_wait_nanoseconds, 900);
        assert_eq!(scheduling.timeslices, 3);
    }

    #[test]
    fn availability_is_exactly_the_presence_of_a_worker_reading() {
        // Stated as an invariant rather than as "the pool is empty", because a sample is also
        // taken for a pool whose workers have all gone. The property that has to hold either way
        // is that a sample claims the counters were read if and only if it read at least one
        // worker -- so a dashboard can trust `available` to mean "these numbers were measured".
        let mine = super::current_thread_id();
        for workers in [vec![], vec![mine]] {
            let scheduling = super::worker_scheduling(&workers);
            assert_eq!(
                scheduling.available,
                scheduling.threads > 0,
                "available={} disagrees with threads={} for {workers:?}",
                scheduling.available,
                scheduling.threads,
            );
            if scheduling.available {
                // The worst worker's wait cannot exceed the pool's total, or the fold is wrong.
                // Nothing is asserted about how much time was accumulated: these are counters
                // since thread start, and the thread sampled here may be one that was only just
                // created.
                assert!(
                    scheduling.worst_runqueue_wait_nanoseconds
                        <= scheduling.runqueue_wait_nanoseconds
                );
            } else {
                // Nothing was read, so every figure is the absence of a measurement, not a
                // measurement of absence.
                assert_eq!(scheduling.runqueue_wait_nanoseconds, 0);
                assert_eq!(scheduling.worst_runqueue_wait_nanoseconds, 0);
            }
        }
    }

    #[test]
    fn a_thread_that_is_not_there_yields_no_attribution_rather_than_all_other() {
        // Reading thread 0 fails, which is the same shape as the controller thread having exited.
        let attribution = super::fault_attribution(&crate::procfs::FaultStats::default(), 0);
        assert!(!attribution.controller_found);
        assert_eq!(attribution.other_minor_faults, 0);
        assert_eq!(attribution.other_major_faults, 0);
    }

    #[test]
    fn the_calling_thread_can_identify_itself() {
        let tid = super::current_thread_id();
        assert_ne!(tid, 0);
        // A tid that is not in /proc would make every subsequent read fail.
        let reading = crate::procfs::thread_scheduling(tid);
        assert!(
            reading.is_ok(),
            "thread {tid} should have scheduler statistics: {reading:?}"
        );
    }
}
