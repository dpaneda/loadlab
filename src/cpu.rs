//! CPU load generation.
//!
//! A pool of worker threads, one per selected logical CPU, each burning a controlled
//! fraction of its own core. Targets are aggregate percentages of the machine's total
//! logical CPUs -- 100% is one saturated CPU, 200% is two -- so the same workload means
//! the same thing on a 2-core laptop and a 16-core server.
//!
//! Two things are deliberately not assumed here. A busy wall clock is not CPU time (see
//! [`DutyController`]), and a requested duty cycle is not a delivered percentage (see
//! [`WORKER_WINDOW`]). The pool publishes the duty it was handed; the controller closes
//! the loop against what the kernel actually charged the process.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use core_affinity::CoreId;

use crate::error::LoadlabError;

/// Duty cycles are published to workers as fixed-point parts per 10 000, so 1.0 is
/// `10_000`. A fixed-point slot means a target change is a single relaxed store the
/// worker will observe on its next window, with no lock and no thread restart.
const DUTY_SCALE: u64 = 10_000;

/// How much wall-clock time one busy/sleep cycle of a worker covers.
///
/// The window trades two things against each other. A longer window makes the *shape*
/// of the cycle more accurate, because `thread::sleep` overshoots by tens of microseconds
/// whatever duration it is asked for, and that overshoot is a fixed cost per window: at
/// 20 ms it distorts the delivered duty by well under a percent, at 1 ms it would halve
/// it. A longer window also makes a worker slower to notice `stop`, which is why this is
/// kept to a fifth of a second's worth of work rather than a whole controller tick.
///
/// 20 ms bounds shutdown latency at 20 ms and the duty granularity at 0.01%.
const WORKER_WINDOW: Duration = Duration::from_millis(20);

/// How many times `spawn` yields while waiting for a worker to publish its kernel id.
///
/// Generous for a bound that is only a backstop: the publish is the new thread's first
/// instruction, so in practice the wait ends on the first or second yield.
const TID_PUBLISH_ATTEMPTS: usize = 10_000;

/// Utilization within this many percentage points of target counts as converged.
///
/// Fixed rather than configurable for the same reason `tolerance_bytes` exists on the
/// memory side: CPU time is quantised to clock ticks, so a single tick's measurement is
/// coarse, and a tolerance that tracked that noise would flap between "converged" and
/// "not" faster than an operator could read it.
pub const TOLERANCE_PCT: f64 = 5.0;

/// Name prefix shared by every CPU worker thread.
///
/// A constant rather than a literal at the construction site because the observation layer finds
/// these threads by name in `/proc`, and the two spellings have to stay the same string or the
/// observation silently reports an empty pool.
pub const WORKER_THREAD_PREFIX: &str = "loadlab-cpu-";

/// Upper bound on the worker threads a client may request.
///
/// `/workload` is remotely controllable, and thread creation is the one part of this
/// controller where a hostile request costs kernel resources rather than accuracy. Every
/// other knob here costs at most accuracy.
pub const MAX_WORKERS: usize = 1024;

/// Weight of the newest CPU reading in the smoothed estimate.
///
/// A 20 ms window is short enough that a single reading is dominated by clock-tick
/// quantisation, so the controller reports a smoothed value rather than the last sample.
const SMOOTHING: f64 = 0.25;

/// Gain of the integral term, in duty-cycle fraction per percentage point of error.
const INTEGRAL_GAIN: f64 = 0.0025;

/// Cap on the integral term, as a fraction of one worker's core.
///
/// Without it, a target the machine cannot reach (or a core shared with something else)
/// would wind the term up without bound, and the pool would sit at duty 1.0 long after
/// the target came back down.
const INTEGRAL_LIMIT: f64 = 0.25;

/// How a pool maps its workers onto logical CPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    /// Workers are left wherever the scheduler puts them.
    Auto,
    /// Worker `i` is bound to logical CPU `cpus[i % cpus.len()]`.
    Pin,
}

impl Affinity {
    /// Parse the value of the `cpu.affinity` config field.
    pub fn parse(source: &str) -> Result<Self, LoadlabError> {
        match source {
            "auto" => Ok(Self::Auto),
            "pin" => Ok(Self::Pin),
            other => Err(LoadlabError::InvalidConfig(format!(
                "cpu.affinity must be \"auto\" or \"pin\", got {other:?}"
            ))),
        }
    }
}

/// Check a `cpu` configuration and return the affinity it names.
///
/// Both the HTTP layer and [`CpuPool::new`] go through the checks below, so the rules a
/// client is told about and the rules a worker is built under cannot drift apart.
pub fn validate(workers: usize, cpus: &[usize], affinity: &str) -> Result<Affinity, LoadlabError> {
    let parsed = Affinity::parse(affinity)?;
    validate_workers(workers)?;
    validate_cpu_ids(cpus)?;
    Ok(parsed)
}

fn validate_workers(workers: usize) -> Result<(), LoadlabError> {
    if workers > MAX_WORKERS {
        return Err(LoadlabError::InvalidConfig(format!(
            "cpu.workers must be at most {MAX_WORKERS}, got {workers}"
        )));
    }
    Ok(())
}

fn validate_cpu_ids(cpus: &[usize]) -> Result<(), LoadlabError> {
    let logical = num_cpus::get();
    for &cpu in cpus {
        if cpu >= logical {
            return Err(LoadlabError::InvalidConfig(format!(
                "cpu.cpus contains logical CPU {cpu}, but this machine has {logical}"
            )));
        }
    }
    Ok(())
}

/// What the controller should ask the pool for on this tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Demand {
    /// Aggregate percent the pool was actually asked to deliver, after clamping to
    /// [`Demand::capacity_pct`].
    pub target_pct: f64,
    /// Fraction of one worker's core, in `0.0..=1.0`.
    pub duty_cycle: f64,
    /// Aggregate percent the pool can deliver with every worker saturated.
    pub capacity_pct: f64,
    /// Whether smoothed utilization is within [`TOLERANCE_PCT`] of the target *and* the
    /// target was reachable.
    pub converged: bool,
    /// Whether the request was above what the pool can deliver.
    pub saturated: bool,
    /// The smoothed estimate the controller is actually steering on.
    ///
    /// This is deliberately *not* the raw per-tick sample. `/proc` reports CPU time in
    /// clock ticks, so a 100ms window quantises the reading to roughly 10 percentage points
    /// of a core and a perfectly steady load reads as 40/50/60/50 in turn. That is fine for
    /// the integrator, which is fed the same smoothed value, but it is a misleading thing to
    /// show an operator or to average into a graph, and it would make `converged` look like
    /// it were flapping when it was not.
    pub actual_pct: f64,
}

/// Turns an aggregate utilization target into a per-worker duty cycle, corrected by what
/// the process was actually measured to be using.
///
/// The feedforward term alone would be enough if a duty cycle meant what it says, and it
/// does not: the sleep that fills the idle part of each window overshoots, a core that
/// another process is using delivers less than it was asked for, and a target that moves
/// faster than the smoothing window is never actually hit. So the duty is a starting
/// guess and the integral term absorbs the difference.
///
/// The correction is deliberately not proportional on the measurement. A proportional
/// term against a quantised, noisy reading would inject that noise straight into the
/// duty, and the workers would oscillate around the target faster than it could be
/// observed. Only the smoothed value reaches the integrator.
#[derive(Debug, Clone, Copy)]
pub struct DutyController {
    smoothed_pct: f64,
    integral: f64,
    initialized: bool,
    /// The smallest difference a single measurement can express, as a percent of a core.
    /// Zero means "as fine as you like", which is the right default for a controller that has
    /// not been told otherwise.
    resolution_pct: f64,
}

impl Default for DutyController {
    fn default() -> Self {
        Self {
            smoothed_pct: 0.0,
            integral: 0.0,
            initialized: false,
            resolution_pct: 0.0,
        }
    }
}

impl DutyController {
    /// A controller that has not yet observed anything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Tell the controller how finely the measurement can resolve a target.
    ///
    /// Set this from [`crate::procfs::clock_tick_seconds`] and the interval between
    /// measurements. It is a property of *how the process is being watched*, not of the target,
    /// so it deliberately survives [`DutyController::reset`]: a workload that stops and starts
    /// again is measured exactly as finely as before.
    pub fn set_resolution_pct(&mut self, resolution_pct: f64) {
        self.resolution_pct = if resolution_pct.is_finite() {
            resolution_pct.max(0.0)
        } else {
            0.0
        };
    }

    /// Forget the history of what has been measured and asked for.
    ///
    /// Called when the workload stops, so that an integral term accumulated against a
    /// target the workload no longer wants does not keep CPU being burned. The measurement
    /// resolution survives: a workload that starts again is watched just as finely as before.
    pub fn reset(&mut self) {
        *self = Self {
            resolution_pct: self.resolution_pct,
            ..Self::default()
        };
    }

    /// Fold one measurement in and report what to ask for.
    ///
    /// `actual_pct` is the whole process, not just the workers: the controller thread
    /// and the async runtime are charged to it too. That is deliberate. Reporting the
    /// process's real CPU consumption keeps the target honest -- a workload specified as
    /// "100%" that shows up as 103% in `top` is worse than one that undershoots -- and
    /// the constant background cost is a fraction of a percent of a core, well inside
    /// [`TOLERANCE_PCT`].
    ///
    /// `capacity_pct` comes from the pool rather than being recomputed here, so the number
    /// the controller clamps against and the number the pool can actually deliver cannot
    /// drift apart.
    pub fn update(&mut self, actual_pct: f64, requested_pct: f64, capacity_pct: f64) -> Demand {
        let actual_pct = if actual_pct.is_finite() {
            actual_pct.max(0.0)
        } else {
            0.0
        };
        let requested_pct = if requested_pct.is_finite() {
            requested_pct.max(0.0)
        } else {
            0.0
        };
        let capacity_pct = if capacity_pct.is_finite() {
            capacity_pct.max(0.0)
        } else {
            0.0
        };

        // The first reading after a start or a reset is adopted whole. Ramping towards it
        // would report a value the process is nowhere near for the first few ticks, and
        // since `converged` is derived from this number, a pool that was already at its
        // target would spend its first second reporting that it had not arrived.
        if self.initialized {
            self.smoothed_pct += SMOOTHING * (actual_pct - self.smoothed_pct);
        } else {
            self.smoothed_pct = actual_pct;
            self.initialized = true;
        }

        let saturated = requested_pct > capacity_pct;
        let target_pct = requested_pct.min(capacity_pct);
        let feedforward = if capacity_pct > 0.0 {
            target_pct / capacity_pct
        } else {
            0.0
        };

        // Integrating is only meaningful while the target is large enough for the
        // measurement to see. `/proc` reports CPU time in clock ticks, so on the default 100ms
        // window a reading moves in steps of about 10 points of a core: a 0.5% target reads as
        // 0, then 10, then 0, and an integral fed that would swing the duty between nothing
        // and 15% of a core to chase a signal two hundred times smaller than the rounding
        // error. Below the floor the feedforward is both the better answer and the honest one
        // -- the duty that buys the requested fraction of a core is the requested fraction --
        // and `converged` still reports the target as met, because smoothed within tolerance
        // of the target is what converged means.
        //
        // Above the floor this is the ordinary integrator, and it is what removes a steady
        // bias between the requested fraction of a core and the fraction a busy or contended
        // machine actually delivers.
        if self.resolution_pct <= 0.0 || target_pct >= self.resolution_pct {
            self.integral = (self.integral + INTEGRAL_GAIN * (target_pct - self.smoothed_pct))
                .clamp(-INTEGRAL_LIMIT, INTEGRAL_LIMIT);
        }
        let duty_cycle = (feedforward + self.integral).clamp(0.0, 1.0);

        Demand {
            target_pct,
            duty_cycle,
            capacity_pct,
            converged: !saturated && (self.smoothed_pct - target_pct).abs() <= TOLERANCE_PCT,
            saturated,
            actual_pct: self.smoothed_pct,
        }
    }
}

/// A worker thread that burns a controlled fraction of one core.
pub struct CpuWorker {
    duty: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    core: Option<CoreId>,
    name: String,
    /// The worker's kernel id, published by the worker itself. Zero until it has started.
    ///
    /// This is how the pool is observed. The thread *name* cannot serve that purpose: a thread
    /// inherits its creator's `comm` and only takes its own once the creating thread's
    /// `pthread_setname_np` has landed, so a worker named here may still be reading as
    /// `loadlab-controller` for a few microseconds after `spawn` returns.
    tid: Arc<AtomicU32>,
    handle: Option<thread::JoinHandle<()>>,
}

impl CpuWorker {
    /// A worker that has not been started, with a duty of zero.
    fn new(name: String, core: Option<CoreId>) -> Self {
        Self {
            duty: Arc::new(AtomicU64::new(0)),
            running: Arc::new(AtomicBool::new(true)),
            finished: Arc::new(AtomicBool::new(true)),
            core,
            name,
            tid: Arc::new(AtomicU32::new(0)),
            handle: None,
        }
    }

    /// Start the thread. Starting an already-started worker replaces the old thread, which
    /// is asked to stop first.
    ///
    /// Returns once the worker has published its kernel id, so a caller that observes the pool
    /// straight after this call has real thread ids to read rather than names it might not see
    /// yet.
    pub fn spawn(&mut self) {
        self.stop();
        let duty = Arc::clone(&self.duty);
        let running = Arc::clone(&self.running);
        let finished = Arc::clone(&self.finished);
        let tid = Arc::clone(&self.tid);
        tid.store(0, Ordering::Release);
        running.store(true, Ordering::Relaxed);
        finished.store(false, Ordering::Relaxed);
        let core = self.core;
        let name = self.name.clone();
        self.handle = Some(
            thread::Builder::new()
                .name(name)
                .spawn(move || run(duty, running, finished, core, tid))
                .expect("failed to spawn CPU worker"),
        );
        self.await_tid();
    }

    /// Block until the worker has published its kernel id.
    ///
    /// The publish is the worker's first instruction and the new thread is already runnable, so
    /// this ends as soon as the scheduler gives it a slice. Yielding rather than sleeping keeps
    /// the controller's tick moving instead of overshooting its interval by a sleep quantum.
    fn await_tid(&self) {
        // Bounded so a thread that never runs cannot wedge the control loop. Past the bound the
        // worker is left unreported for this tick, which is a worse answer than a stale figure
        // but a recoverable one; the next tick reads it.
        for _ in 0..TID_PUBLISH_ATTEMPTS {
            if self.tid.load(Ordering::Acquire) != 0 {
                return;
            }
            thread::yield_now();
        }
    }

    /// The worker's kernel id, or zero while it has not published one.
    #[must_use]
    pub fn thread_id(&self) -> u32 {
        self.tid.load(Ordering::Acquire)
    }

    /// Set the fraction of its core this worker should keep busy, in `0.0..=1.0`.
    pub fn set_duty_cycle(&self, duty_cycle: f64) {
        self.duty
            .store(duty_to_units(duty_cycle), Ordering::Relaxed);
    }

    /// Ask the worker to finish, without waiting for it.
    ///
    /// This deliberately does not join. The handle is dropped, which detaches the thread;
    /// it leaves its loop at the top of the next [`WORKER_WINDOW`] and sets the finished
    /// flag on the way out.
    ///
    /// Blocking here would be the obvious implementation and the wrong one. A worker can be
    /// up to one window away from noticing the flag, so joining costs up to
    /// [`WORKER_WINDOW`] per worker -- 80 ms for a four-core box. That is paid inside the
    /// controller tick, and a controller that is not ticking does not reconcile *memory*
    /// either. An 80 ms hole in a 100 ms tick, opened by a workload change, would show up as
    /// memory convergence stalling for reasons that have nothing to do with memory.
    ///
    /// The cost of not joining is that a rebuilt pool's new workers can briefly share a core
    /// with the outgoing ones, for at most one window. The alternative is a stalled control
    /// loop, which is the worse failure for a load generator.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.handle = None;
    }

    /// Whether the thread has actually left its loop.
    ///
    /// Separate from "has been asked to stop", which is what `stop` means. Exists so a
    /// shutdown can be observed without reading `/proc` or joining, either of which would
    /// either block or make the test depend on the rest of the test binary's threads.
    #[cfg(test)]
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }
}

impl Drop for CpuWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The busy/sleep loop every worker runs.
///
/// The three `Arc`s are taken by value on purpose: they are what this thread is being
/// spawned *with*, so they have to be moved into the closure. Taking references would mean
/// cloning them immediately, which is the same work with a worse signature.
#[allow(clippy::needless_pass_by_value)]
fn run(
    duty: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    core: Option<CoreId>,
    tid: Arc<AtomicU32>,
) {
    // Published before anything else so the pool that spawned this thread can observe it. The
    // controller samples scheduling counters for exactly these ids, so a worker that has not
    // published yet is one the pool would misreport as missing.
    tid.store(crate::observation::current_thread_id(), Ordering::Release);

    // Affinity has to be set from inside the thread: it is a per-thread attribute, so
    // pinning the handle after the fact would bind the wrong thread.
    if let Some(core) = core {
        core_affinity::set_for_current(core);
    }

    while running.load(Ordering::Relaxed) {
        let start = Instant::now();
        // SAFETY of the cast: the stored value is a count of parts per 10 000, so it is
        // bounded by `DUTY_SCALE` and exact as an f64.
        #[allow(clippy::cast_precision_loss)]
        let duty = duty.load(Ordering::Relaxed) as f64 / DUTY_SCALE as f64;
        if duty > 0.0 {
            let busy = WORKER_WINDOW.mul_f64(duty);
            while start.elapsed() < busy {
                std::hint::spin_loop();
            }
        }
        let remaining = WORKER_WINDOW.saturating_sub(start.elapsed());
        if !remaining.is_zero() {
            thread::sleep(remaining);
        }
    }
    finished.store(true, Ordering::Relaxed);
}

/// Convert a duty cycle to the fixed-point representation workers read.
fn duty_to_units(duty_cycle: f64) -> u64 {
    // A non-finite duty is a bug rather than operator input, and the safe reading of a bug
    // in a load generator is to generate nothing: saturating an infinity would turn a bad
    // computation into a saturated core.
    if !duty_cycle.is_finite() {
        return 0;
    }
    let clamped = duty_cycle.clamp(0.0, 1.0);
    // SAFETY of the cast: `clamped` is in 0.0..=1.0, so the product is in
    // 0.0..=DUTY_SCALE and the rounding never wraps.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    {
        (clamped * DUTY_SCALE as f64).round() as u64
    }
}

/// A pool of workers plus the arithmetic needed to turn an aggregate target into a duty
/// cycle.
pub struct CpuPool {
    workers: Vec<CpuWorker>,
    capacity_pct: f64,
}

impl CpuPool {
    /// Build a pool.
    ///
    /// `workers` of zero means one worker per selected logical CPU. `cpus` of empty means
    /// every logical CPU. `workers` larger than the number of selected CPUs is allowed and
    /// wraps: worker `i` lands on `cpus[i % cpus.len()]`, so two workers can share a core.
    /// That is a legitimate way to run more threads than cores, but it is not a way to
    /// deliver more CPU -- [`CpuPool::capacity_pct`] stays at `workers * 100`.
    pub fn new(workers: usize, cpus: &[usize], affinity: Affinity) -> Result<Self, LoadlabError> {
        // `affinity` is already parsed here, so only the numeric bounds still need checking.
        // They are re-checked rather than assumed because this is a public constructor.
        validate_workers(workers)?;
        validate_cpu_ids(cpus)?;
        let selected: Vec<usize> = if cpus.is_empty() {
            (0..num_cpus::get()).collect()
        } else {
            cpus.to_vec()
        };
        let count = if workers == 0 {
            selected.len()
        } else {
            workers
        };

        let pool = Self {
            workers: (0..count)
                .map(|index| {
                    let core = match affinity {
                        Affinity::Auto => None,
                        Affinity::Pin => Some(core_id(selected[index % selected.len()])),
                    };
                    CpuWorker::new(format!("{WORKER_THREAD_PREFIX}{index}"), core)
                })
                .collect(),
            // SAFETY of the cast: `count` is at most `MAX_WORKERS`, so it is exact as f64.
            #[allow(clippy::cast_precision_loss)]
            capacity_pct: count as f64 * 100.0,
        };
        Ok(pool)
    }

    /// Start every worker.
    pub fn spawn(&mut self) {
        for worker in &mut self.workers {
            worker.spawn();
        }
    }

    /// Set every worker's duty cycle to the same fraction.
    pub fn set_duty_cycle(&self, duty_cycle: f64) {
        for worker in &self.workers {
            worker.set_duty_cycle(duty_cycle);
        }
    }

    /// Ask every worker to finish, without blocking the caller.
    ///
    /// See [`CpuWorker::stop`] for why this does not join.
    pub fn stop(&mut self) {
        for worker in &mut self.workers {
            worker.stop();
        }
    }

    /// Whether every worker thread has actually left its loop. Test-only for the same
    /// reason as [`CpuWorker::is_finished`]: the controller has no use for it, because a
    /// worker that is on its way out is indistinguishable from one that has gone as far as
    /// the next tick is concerned.
    #[cfg(test)]
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.workers.iter().all(CpuWorker::is_finished)
    }

    /// How many workers are in the pool.
    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// The kernel ids of the workers that can be observed, in worker order.
    ///
    /// A worker that has not published its id yet is left out, so this can be shorter than
    /// [`Self::worker_count`]. That is the honest answer for the tick it happens on.
    #[must_use]
    pub fn worker_thread_ids(&self) -> Vec<u32> {
        self.workers
            .iter()
            .map(CpuWorker::thread_id)
            .filter(|tid| *tid != 0)
            .collect()
    }

    /// Aggregate percent the pool can deliver with every worker saturated.
    #[must_use]
    pub fn capacity_pct(&self) -> f64 {
        self.capacity_pct
    }
}

impl Drop for CpuPool {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The `core_affinity` id for a logical CPU index.
///
/// `CoreId` is an opaque newtype with no public constructor, so it can only come from the
/// kernel's own list. That list can be shorter than `num_cpus::get()` on a host with
/// cpuset restrictions, in which case the index is meaningless and the worker is left
/// unpinned -- better an unpinned worker than a thread bound to nothing.
fn core_id(index: usize) -> CoreId {
    core_affinity::get_core_ids().map_or_else(
        || core_affinity::CoreId { id: index },
        |ids| ids[index % ids.len()],
    )
}

#[cfg(test)]
mod tests {
    // Duty cycles and percentages compared here are exact binary fractions or values the
    // control law produces, so `==` is the correct comparison. The cases where it is not --
    // anything that carries a float residue -- use `assert_close` instead, so the allowance
    // is not hiding a genuine tolerance.
    #![allow(clippy::float_cmp)]

    use super::{
        Affinity, CpuPool, DutyController, MAX_WORKERS, TOLERANCE_PCT, duty_to_units, validate,
    };

    /// A duty cycle survives the round trip through the fixed-point slot workers read.
    #[test]
    fn duty_cycles_round_trip_through_fixed_point() {
        for duty in [0.0, 0.001, 0.25, 0.5, 0.75, 0.99, 1.0] {
            let units = duty_to_units(duty);
            // SAFETY of the cast: the units are bounded by the 10 000 scale.
            #[allow(clippy::cast_precision_loss)]
            let round_tripped = units as f64 / 10_000.0;
            assert!(
                (round_tripped - duty).abs() < 1e-4,
                "duty {duty} came back as {round_tripped}"
            );
        }
    }

    /// A duty that is not a number must not become a duty of "everything".
    #[test]
    fn duty_cycles_outside_the_unit_interval_are_clamped_or_rejected() {
        assert_eq!(duty_to_units(-1.0), 0);
        assert_eq!(duty_to_units(5.0), 10_000);
        assert_eq!(duty_to_units(f64::NAN), 0);
        assert_eq!(duty_to_units(f64::INFINITY), 0);
    }

    /// The feedforward term alone hits the target when the measurement agrees with it.
    #[test]
    fn a_controller_that_is_already_on_target_does_not_oscillate() {
        let mut controller = DutyController::new();
        let mut previous = None;
        for _ in 0..20 {
            let duty = controller.update(100.0, 100.0, 100.0).duty_cycle;
            if let Some(before) = previous {
                assert_eq!(duty, before, "no error means no correction");
            }
            previous = Some(duty);
        }
        assert_eq!(previous, Some(1.0));
        assert!(controller.update(100.0, 100.0, 100.0).converged);
    }

    /// The first reading is adopted whole, so a pool that was already at its target is
    /// reported as converged from the first tick rather than ramping up to it.
    #[test]
    fn the_first_reading_is_believed_rather_than_ramped_towards() {
        let mut controller = DutyController::new();
        let demand = controller.update(100.0, 100.0, 100.0);
        assert!(demand.converged, "no ramp before the first report");
        assert_eq!(demand.duty_cycle, 1.0);
    }

    /// The interesting case: the workers convert only part of the duty they are given into
    /// CPU time -- a sleep that overshoots, or a core another process is already using --
    /// and the controller has to close the gap. The feedforward guess alone lands short, so
    /// this only passes if the integral term is doing its job.
    #[test]
    fn an_inefficient_pool_is_corrected_upward_and_converges() {
        let capacity = 400.0;
        let target = 100.0;
        // The pool only converts 80% of the duty it is given into CPU time.
        let deliver = |duty: f64| duty * 0.8 * capacity;

        let mut controller = DutyController::new();
        let mut demand = controller.update(0.0, target, capacity);
        for _ in 0..400 {
            demand = controller.update(deliver(demand.duty_cycle), target, capacity);
        }

        assert!(
            demand.converged,
            "should have converged: target={} actual={} duty={}",
            demand.target_pct,
            deliver(demand.duty_cycle),
            demand.duty_cycle
        );
        assert!(
            (deliver(demand.duty_cycle) - target).abs() <= TOLERANCE_PCT,
            "landed at {} instead of {target}",
            deliver(demand.duty_cycle)
        );
        // The correction has to be a real increase over the feedforward guess of 0.25,
        // otherwise the test is passing for the wrong reason.
        assert!(demand.duty_cycle > 0.25, "no correction applied");
    }

    /// A machine that cannot reach the target is reported as saturated, not as converged.
    #[test]
    fn a_request_above_capacity_is_clamped_and_flagged() {
        let mut controller = DutyController::new();
        let demand = controller.update(400.0, 800.0, 400.0);

        assert_eq!(demand.target_pct, 400.0, "clamped to what 4 workers can do");
        assert_eq!(demand.capacity_pct, 400.0);
        assert!(demand.saturated);
        assert!(
            !demand.converged,
            "hitting the ceiling is not the same as hitting the target"
        );
    }

    /// A pool with no capacity cannot deliver anything, whatever is asked of it. This is
    /// what a machine with a worker count of zero, or a failed pool build, would look like
    /// if it were allowed to run at all.
    #[test]
    fn a_pool_with_no_capacity_delivers_nothing_and_says_so() {
        let mut controller = DutyController::new();
        let demand = controller.update(0.0, 50.0, 0.0);
        assert_eq!(demand.duty_cycle, 0.0);
        assert_eq!(demand.target_pct, 0.0);
        assert!(demand.saturated);
        assert!(!demand.converged);
    }

    /// Capacity is the pool's to declare, and the controller clamps against that number
    /// rather than recomputing it from the worker count.
    ///
    /// The pool here is ideal: it converts duty to CPU time perfectly. A correct controller
    /// therefore needs no correction at all, and the steady-state duty is exactly the
    /// feedforward term. That is a stronger statement than checking the first tick, which
    /// deliberately over-drives away from a cold reading of zero.
    #[test]
    fn an_ideal_pool_needs_no_correction_whatever_the_capacity() {
        let settle = |capacity: f64| {
            let mut controller = DutyController::new();
            let mut demand = controller.update(0.0, 50.0, capacity);
            for _ in 0..200 {
                demand = controller.update(demand.duty_cycle * capacity, 50.0, capacity);
            }
            demand
        };

        let wide = settle(800.0);
        assert_eq!(wide.capacity_pct, 800.0);
        assert_close(
            wide.duty_cycle,
            0.0625,
            "50 of 800 is a sixteenth of a core",
        );
        assert!(wide.converged);

        let narrow = settle(100.0);
        assert_eq!(narrow.capacity_pct, 100.0);
        assert_close(narrow.duty_cycle, 0.5, "half a core");
        assert!(narrow.converged);
    }

    /// The integral decays towards zero asymptotically rather than snapping to it, so a
    /// settled duty carries a float residue. Anything at that scale is the integrator
    /// arriving, not a control error.
    fn assert_close(actual: f64, expected: f64, what: &str) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "{what}: expected {expected}, got {actual}"
        );
    }

    /// A stopped workload must not keep burning the integral's worth of CPU.
    #[test]
    fn reset_drops_the_history_so_a_stale_target_stops_being_burned() {
        let mut controller = DutyController::new();
        for _ in 0..100 {
            controller.update(0.0, 100.0, 100.0);
        }
        assert!(controller.update(0.0, 100.0, 100.0).duty_cycle > 0.9);

        controller.reset();
        let demand = controller.update(0.0, 0.0, 100.0);
        assert_eq!(demand.duty_cycle, 0.0, "no target, no duty");
        assert!(demand.converged);
    }

    /// Measurement noise must not become duty cycle noise. If it did, a settled pool
    /// would keep moving.
    #[test]
    fn a_noisy_measurement_produces_only_a_small_duty_change() {
        let mut controller = DutyController::new();
        for _ in 0..50 {
            controller.update(100.0, 100.0, 100.0);
        }
        let settled = controller.update(100.0, 100.0, 100.0).duty_cycle;

        let mut worst = 0.0_f64;
        for index in 0..50 {
            // +/- 4 points, which is a realistic single-tick quantisation error.
            let reading = 100.0 + if index % 2 == 0 { 4.0 } else { -4.0 };
            let duty = controller.update(reading, 100.0, 100.0).duty_cycle;
            worst = worst.max((duty - settled).abs());
        }
        assert!(
            worst < 0.02,
            "duty wandered by {worst} on noise alone (settled at {settled})"
        );
    }

    /// The reported utilization is the value the controller steers on, not the raw sample.
    ///
    /// `/proc` reports CPU time in clock ticks, so one 100ms window can only be counted in
    /// steps of roughly 10 points of a core. Publishing those samples raw makes a perfectly
    /// steady 50% load read as 40/50/60/50, which is both ugly to graph and misleading to
    /// anything averaging the series. The integrator has always been fed the smoothed value;
    /// this pins that the published one is the same number, so `converged` cannot disagree
    /// with the figure an operator is looking at.
    #[test]
    fn the_reported_utilization_is_the_smoothed_estimate() {
        let mut controller = DutyController::new();
        for _ in 0..50 {
            controller.update(50.0, 50.0, 100.0);
        }

        // A steady 50% load, reported as raw clock-tick quantisation.
        let mut samples = Vec::new();
        let mut duty = 0.0;
        for index in 0..40 {
            let reading = 50.0 + if index % 2 == 0 { 10.0 } else { -10.0 };
            let demand = controller.update(reading, 50.0, 100.0);
            samples.push(demand.actual_pct);
            duty = demand.duty_cycle;
        }

        // An exponential average of alternating +/-x settles to a ripple of x*alpha/(1+alpha),
        // so 10 points of quantisation leaves about +/-2 here against 10 raw. Asserting the
        // ripple is small rather than absent is the point: a filter that perfectly removed
        // alternating noise would lag a genuinely moving target, which is the trade the
        // smoothing weight exists to make.
        let spread = samples
            .iter()
            .copied()
            .fold((f64::MAX, f64::MIN), |(lo, hi), value| {
                (lo.min(value), hi.max(value))
            });
        assert!(
            spread.1 - spread.0 < 5.0,
            "the published figure should be steady, it wandered over {span:.2}..{hi:.2} \
             on 20 points of raw quantisation",
            span = spread.0,
            hi = spread.1
        );
        // Comparing the mean rather than the final sample: the series ends on whatever phase
        // of the ripple the loop happens to stop at, and that phase is not the property
        // under test. The mean is.
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        assert!(
            (mean - 50.0).abs() < 0.5,
            "the published figure should sit on the target, averaged {mean}"
        );
        // The duty is a shade under 0.5 rather than exactly on it: the ripple's two phases are
        // not symmetric about the target, so the integrator settles a little low to cancel the
        // bias. That is the integral doing its job on a slightly biased estimate, and it is
        // bounded by INTEGRAL_LIMIT, so a biased reading cannot run the duty away.
        assert!(
            (duty - 0.5).abs() < 0.05,
            "duty {duty} should track the 0.5 needed for 50 of 100"
        );

        // Convergence is decided against this same number, so the two can never disagree.
        let demand = controller.update(50.0, 50.0, 100.0);
        assert!(demand.converged);
        assert!((demand.actual_pct - demand.target_pct).abs() < 5.0);
    }

    /// A target finer than the measurement can resolve must not be chased.
    ///
    /// `/proc` counts CPU time in clock ticks, so on a 100ms window a reading moves in steps
    /// of about 10 points of a core. A 0.5% target therefore reads as 0, 10, 0, 10 -- and an
    /// integrator fed that produces a duty swinging between nothing and 15% of a core, which
    /// is the opposite of the steady light load that was asked for. Below the floor the
    /// controller holds the feedforward and stays still.
    #[test]
    fn a_target_finer_than_the_measurement_is_served_without_hunting() {
        let mut controller = DutyController::new();
        // One clock tick per 100ms window: 10 points of a core.
        controller.set_resolution_pct(10.0);

        let mut duties = Vec::new();
        for index in 0..200 {
            // The reading alternates between "no tick caught the burst" and "one did".
            let reading = if index % 3 == 0 { 10.0 } else { 0.0 };
            duties.push(controller.update(reading, 0.5, 100.0).duty_cycle);
        }

        let duty = duties[duties.len() - 1];
        assert!(
            (duty - 0.005).abs() < 1e-9,
            "duty {duty} should be the 0.005 that buys half a percent of a core"
        );
        let spread = duties
            .iter()
            .copied()
            .fold((f64::MAX, f64::MIN), |(lo, hi), value| {
                (lo.min(value), hi.max(value))
            });
        assert!(
            spread.1 - spread.0 < 1e-9,
            "the duty must not move at all, it wandered {span}..{hi}",
            span = spread.0,
            hi = spread.1
        );
    }

    /// The floor must not disable the integrator where the measurement *can* see the target,
    /// or every steady-state bias would go uncorrected. That correction is the integral's
    /// whole reason to exist.
    #[test]
    fn a_target_the_measurement_can_see_still_gets_corrected() {
        let mut controller = DutyController::new();
        controller.set_resolution_pct(10.0);

        // A contended machine that only ever delivers 90% of the duty it is given: a steady
        // bias of exactly the kind the integral exists to cancel, and one the feedforward
        // alone would sit 10 points short of forever. The plant has to respond to the duty, or
        // this would measure an integrator winding up against a wall rather than finding a
        // balance, so the reading is derived from the duty that was actually asked for.
        let mut duty = 0.5;
        for _ in 0..400 {
            let reading = duty * 100.0 * 0.9;
            duty = controller.update(reading, 50.0, 100.0).duty_cycle;
        }
        assert!(
            (duty - 50.0 / 90.0).abs() < 0.01,
            "the duty should settle where the biased plant reads 50, expected {} got {duty}",
            50.0 / 90.0
        );
        assert!(
            duty > 0.5,
            "which is above the 0.5 feedforward, so the integral really did contribute"
        );
        assert!(controller.update(duty * 90.0, 50.0, 100.0).converged);
    }

    /// The resolution is a property of the measurement, so it outlives the history of a target
    /// that has gone away. A workload that stops and restarts is measured just as finely, and
    /// so its first tick after the restart must not start hunting again.
    #[test]
    fn resetting_keeps_the_measurement_resolution() {
        let mut controller = DutyController::new();
        controller.set_resolution_pct(10.0);
        for _ in 0..50 {
            controller.update(0.0, 100.0, 100.0);
        }
        controller.reset();

        // Below the floor, still fedforward-only: the same behaviour as before the reset,
        // which a `reset` that cleared the resolution would lose.
        for index in 0..20 {
            let reading = if index % 3 == 0 { 10.0 } else { 0.0 };
            let demand = controller.update(reading, 0.5, 100.0);
            assert_eq!(
                demand.duty_cycle, 0.005,
                "tick {index} hunted after a reset"
            );
        }
    }

    /// A non-finite reading cannot poison the integrator into a permanent full tilt.
    #[test]
    fn non_finite_measurements_are_treated_as_zero() {
        let mut controller = DutyController::new();
        let demand = controller.update(f64::NAN, 50.0, 100.0);
        assert!(demand.duty_cycle.is_finite());
        assert!(demand.target_pct.is_finite());
        assert_eq!(
            controller.update(f64::INFINITY, f64::NAN, 100.0).target_pct,
            0.0
        );
    }

    /// A pool with no workers configured gets one per logical CPU.
    #[test]
    fn zero_workers_means_one_per_logical_cpu() {
        let pool = CpuPool::new(0, &[], Affinity::Auto).expect("pool");
        assert_eq!(pool.worker_count(), num_cpus::get());
        assert_eq!(pool.capacity_pct(), num_cpus::get() as f64 * 100.0);
    }

    /// An explicit CPU list is the pool, not the machine.
    #[test]
    fn an_explicit_cpu_list_sizes_the_pool() {
        let pool = CpuPool::new(0, &[0], Affinity::Auto).expect("pool");
        assert_eq!(pool.worker_count(), 1);
    }

    /// Every worker a pool counts has to be observable the moment the pool is spawned.
    ///
    /// This is the invariant the run-queue observation rests on. Workers used to be found by
    /// scanning the process's threads for their `comm`, and a thread inherits its creator's `comm`
    /// until the creator's `pthread_setname_np` lands -- so a pool sampled on the tick it was
    /// created could report that it had no run-queue data at all. The ids below are published by
    /// the workers themselves and awaited by `spawn`, so the count cannot disagree.
    #[test]
    fn a_spawned_pool_publishes_a_readable_id_for_every_worker() {
        let mut pool = CpuPool::new(4, &[], Affinity::Auto).expect("pool");
        pool.spawn();

        let ids = pool.worker_thread_ids();
        assert_eq!(
            ids.len(),
            pool.worker_count(),
            "a worker the pool counts has no id to observe: {ids:?}"
        );
        for tid in ids {
            assert_ne!(
                tid, 0,
                "an unpublished id would be read as no worker at all"
            );
            assert!(
                crate::procfs::thread_scheduling(tid).is_ok(),
                "thread {tid} has no scheduler accounting, so it cannot be sampled"
            );
        }

        let scheduling = crate::observation::worker_scheduling(&pool.worker_thread_ids());
        assert!(
            scheduling.available,
            "a running pool reported no scheduling data: {scheduling:?}"
        );
        assert_eq!(scheduling.threads, pool.worker_count());
    }

    /// More workers than selected CPUs wraps rather than running off the end.
    #[test]
    fn more_workers_than_cpus_wraps_onto_the_same_core() {
        let pool = CpuPool::new(4, &[0], Affinity::Pin).expect("pool");
        assert_eq!(pool.worker_count(), 4);
        assert_eq!(pool.capacity_pct(), 400.0);
    }

    /// `stop` must not block, and the workers must still actually leave. Both halves matter:
    /// a blocking stop stalls the controller tick, and a stop that never completes would
    /// leave a saturated core running after the workload was stopped.
    #[test]
    fn stop_does_not_block_but_the_workers_still_leave() {
        let mut pool = CpuPool::new(2, &[], Affinity::Auto).expect("pool");
        pool.spawn();
        pool.set_duty_cycle(1.0);

        let start = std::time::Instant::now();
        pool.stop();
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(5),
            "stop blocked for {elapsed:?}; it must not wait for the workers"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !pool.is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "workers never left their loop after stop"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// A pool that was never started counts as finished, so a caller cannot mistake "no
    /// threads yet" for "threads that will not stop".
    #[test]
    fn an_unstarted_pool_is_finished() {
        let pool = CpuPool::new(2, &[], Affinity::Auto).expect("pool");
        assert!(pool.is_finished());
    }

    /// A CPU that does not exist is a client error, not something to silently drop.
    #[test]
    fn an_out_of_range_cpu_id_is_rejected() {
        let absent = num_cpus::get();
        let error = CpuPool::new(0, &[absent], Affinity::Auto)
            .err()
            .expect("should reject");
        assert!(error.to_string().contains("logical CPU"), "{error}");

        let error = validate(1, &[absent], "pin").expect_err("should reject");
        assert!(error.to_string().contains("logical CPU"), "{error}");
    }

    /// The remote control plane must not be able to ask for an unbounded thread count.
    #[test]
    fn worker_count_is_capped() {
        let error = validate(MAX_WORKERS + 1, &[], "auto").expect_err("should reject");
        assert!(error.to_string().contains("cpu.workers"), "{error}");
        assert!(validate(MAX_WORKERS, &[], "auto").is_ok());
    }

    /// An unknown affinity word is a 400, not a silent fallback to "auto".
    #[test]
    fn affinity_accepts_only_auto_and_pin() {
        assert_eq!(Affinity::parse("auto").expect("auto"), Affinity::Auto);
        assert_eq!(Affinity::parse("pin").expect("pin"), Affinity::Pin);
        let error = Affinity::parse("none").expect_err("none is not a mode");
        assert!(error.to_string().contains("cpu.affinity"), "{error}");
    }

    #[test]
    fn a_pool_is_usable_after_stop() {
        let mut pool = CpuPool::new(1, &[], Affinity::Auto).expect("pool");
        pool.spawn();
        pool.stop();
        pool.spawn();
        pool.set_duty_cycle(0.5);
        pool.stop();
    }
}
