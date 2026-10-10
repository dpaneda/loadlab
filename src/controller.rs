use std::{
    sync::{
        Arc, PoisonError, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use tracing::{error, warn};

use crate::{
    cpu::{Affinity, CpuPool, DutyController},
    error::LoadlabError,
    file_backed::FileBackedController,
    io::IoController,
    memory::AnonymousArena,
    memory_access::AccessController,
    network::NetworkController,
    observation, procfs,
    setting::{Dimension, ExpressionCache},
    state::{CpuRuntime, RuntimeStatus, WorkloadState},
};

/// Name of the controller thread.
///
/// A constant because the observation layer attributes page faults to this thread by name: it is the
/// thread that touches the anonymous arena, the file-backed mapping and the memory-access window, so
/// its faults are the ones loadlab generated on purpose.
pub const CONTROLLER_THREAD_NAME: &str = "loadlab-controller";

/// Start the controller thread.
///
/// The CPU pool is built and torn down here rather than handed in, because whether a pool
/// should exist at all is a decision the controller makes: threads are only running while
/// the workload is, and a change to the worker count or the CPU selection has to rebuild
/// them. Passing a pool in from `main` would mean constructing threads nobody had asked
/// for yet and leaving them to the caller's teardown path.
///
/// `max_file_backed_bytes`, `max_shared_bytes`, `max_io_bytes`, and `max_network_ops_per_second`
/// are process-level ceilings, so they are passed in rather than read from `WorkloadState`: a client
/// can ask for file-backed memory, shared memory, disk I/O, or network traffic but cannot lift what
/// the operator allowed. The shared ceiling in particular has to be separate, because the cost of a
/// shared workload is `private_bytes * actors` and no single requested field reveals that; the
/// network ceiling is separate because traffic leaves the host. See `api::Limits` for why these are
/// not part of the desired state.
pub fn spawn(
    state: Arc<RwLock<WorkloadState>>,
    runtime: Arc<RwLock<RuntimeStatus>>,
    max_file_backed_bytes: u64,
    file_backed_dir: std::path::PathBuf,
    max_shared_bytes: u64,
    max_io_bytes: u64,
    io_dir: std::path::PathBuf,
    max_network_ops_per_second: u64,
    io_latency_histograms: crate::metrics::IoLatencyHistograms,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name(CONTROLLER_THREAD_NAME.to_owned())
        .spawn(move || {
            // Resolved once, here, because this thread is the one whose page faults loadlab
            // generates: it is what touches the arena, the file-backed mapping and the access window.
            // Looking it up per tick would mean re-scanning every thread's name to rediscover a fact
            // the thread already knows.
            let controller_tid = observation::current_thread_id();

            if !procfs::thread_scheduling_available() {
                // Once, at startup. Run-queue wait is the one observation a kernel can withhold,
                // and it withholds it silently, so an operator looking at a flat zero later deserves
                // to have been told why at the time they could act on it.
                tracing::info!(
                    "this kernel has no CONFIG_SCHEDSTATS: CPU run-queue wait will be reported as \
                     unavailable rather than as zero"
                );
            }

            let mut arena = match AnonymousArena::new() {
                Ok(arena) => arena,
                Err(error) => {
                    error!(%error, "failed to initialize anonymous memory arena");
                    if let Ok(mut runtime) = runtime.write() {
                        runtime.last_error = Some(error.to_string());
                    }
                    return;
                }
            };

            // One compilation cache for every expression-valued field. The key includes the
            // dimension and the source text, so two fields that spell the same expression share a
            // compilation and an expression that does not change is never recompiled -- a parser
            // has no place in the control loop.
            let mut expressions = ExpressionCache::default();
            let mut pool: Option<CpuPool> = None;
            let mut pool_spec: Option<PoolSpec> = None;
            let mut duty = DutyController::new();
            let mut measurement: Option<Measurement> = None;
            let mut access: Option<AccessController> = None;
            let mut access_spec: Option<AccessSpec> = None;
            let mut file_backed: Option<FileBackedController> = None;
            let mut shared = crate::shared::SharedController::new();
            let mut io: Option<IoController> = None;
            let mut network: Option<NetworkController> = None;

            while !shutdown.load(Ordering::Relaxed) {
                let tick_start = Instant::now();
                let config = match state.read() {
                    Ok(value) => value.clone(),
                    Err(poisoned) => poisoned.into_inner().clone(),
                };

                let baseline = runtime
                    .read()
                    .map_or(0, |status| status.baseline_rss_anon_bytes);
                // `t` for expressions comes from the experiment clock, not from process uptime. The
                // clock re-anchors when the workload starts or when `experiment.start_time` changes,
                // so this is the only place in a tick where the identity of the run is decided.
                //
                // A write lock, because the clock moves in place. The alternative -- read it, decide
                // whether it moved, then take a second lock to move it -- leaves a window in which
                // two ticks could disagree about when the run began, which for a 1ms tick is not
                // theoretical. Nothing in this critical section blocks: two clock reads.
                let elapsed_seconds = {
                    let mut status = runtime.write().unwrap_or_else(PoisonError::into_inner);
                    if let crate::experiment::Observed::Rejected(error) = status
                        .experiment
                        .observe(config.running, config.experiment.start_time.as_deref())
                    {
                        // Unreachable through the API, which parses the timestamp on the way in, so
                        // reaching this means a bug rather than a client mistake. The run continues
                        // from its start rather than stopping: a workload lab that refuses to run
                        // because of an unreadable timestamp is worse than one that says `t` is the
                        // age of the run.
                        warn!(
                            %error,
                            "experiment.start_time could not be used; measuring this run from the \
                             moment it started instead"
                        );
                    }
                    status.experiment.elapsed_seconds()
                };
                // Read before taking the write lock below: this is a sysfs read, and the lock
                // should not be held across I/O.
                let thp_host = procfs::thp_host_policy();

                let memory = reconcile_memory(
                    &config,
                    baseline,
                    elapsed_seconds,
                    &mut arena,
                    &mut expressions,
                );
                let cpu = reconcile_cpu(
                    &config,
                    elapsed_seconds,
                    &mut pool,
                    &mut pool_spec,
                    &mut duty,
                    &mut expressions,
                    &mut measurement,
                );

                let mut tick_error = None;
                let cpu_runtime = match cpu {
                    Ok(cpu) => cpu,
                    Err(error) => {
                        warn!(%error, "CPU reconciliation failed");
                        tick_error = Some(error.to_string());
                        CpuRuntime::default()
                    }
                };
                let access_runtime = match reconcile_access(
                    &config,
                    elapsed_seconds,
                    &mut access,
                    &mut access_spec,
                    controller_tid,
                    &mut expressions,
                ) {
                    Ok(ar) => ar,
                    Err(error) => {
                        warn!(%error, "memory-access reconciliation failed");
                        tick_error = Some(error.to_string());
                        crate::state::MemoryAccessRuntime::default()
                    }
                };
                let file_backed_runtime = match reconcile_file_backed(
                    &config,
                    elapsed_seconds,
                    max_file_backed_bytes,
                    &file_backed_dir,
                    &mut file_backed,
                    &mut expressions,
                ) {
                    Ok(fr) => fr,
                    Err(error) => {
                        warn!(%error, "file-backed reconciliation failed");
                        tick_error = Some(error.to_string());
                        crate::state::FileBackedRuntime::default()
                    }
                };
                let shared_runtime = match reconcile_shared(
                    &config,
                    elapsed_seconds,
                    max_shared_bytes,
                    &mut shared,
                    &mut expressions,
                ) {
                    Ok(sr) => sr,
                    Err(error) => {
                        warn!(%error, "shared-memory reconciliation failed");
                        tick_error = Some(error.to_string());
                        crate::state::SharedRuntime::default()
                    }
                };
                let io_runtime = match reconcile_io(
                    &config,
                    elapsed_seconds,
                    max_io_bytes,
                    &io_dir,
                    &mut io,
                    &mut expressions,
                    &io_latency_histograms,
                ) {
                    Ok(ir) => ir,
                    Err(error) => {
                        warn!(%error, "i/o reconciliation failed");
                        tick_error = Some(error.to_string());
                        crate::state::IoRuntime::default()
                    }
                };
                let network_runtime = match reconcile_network(
                    &config,
                    elapsed_seconds,
                    max_network_ops_per_second,
                    &mut network,
                    &mut expressions,
                ) {
                    Ok(nr) => nr,
                    Err(error) => {
                        warn!(%error, "network reconciliation failed");
                        tick_error = Some(error.to_string());
                        crate::state::NetworkRuntime::default()
                    }
                };

                if let Ok(mut runtime) = runtime.write() {
                    runtime.last_tick_at = Some(tick_start);
                    runtime.tick_count = runtime.tick_count.saturating_add(1);
                    runtime.arena_resident_bytes = arena.resident_bytes() as u64;
                    runtime.cpu = cpu_runtime;
                    runtime.memory_access = access_runtime;
                    runtime.file_backed = file_backed_runtime;
                    // Published every tick so a scrape can read the ceiling even before any
                    // file-backed workload exists, which is when a dashboard first wants it.
                    runtime.file_backed_ceiling_bytes = max_file_backed_bytes;
                    runtime.shared = shared_runtime;
                    // Published every tick for the same reason as the file-backed ceiling: a
                    // dashboard wants the limit before any shared workload exists.
                    runtime.shared_ceiling_bytes = max_shared_bytes;
                    runtime.io = io_runtime;
                    // Published every tick for the same reason as the other ceilings: a dashboard
                    // wants the limit before any I/O workload exists.
                    runtime.io_ceiling_bytes = max_io_bytes;
                    runtime.network = network_runtime;
                    // Published every tick for the same reason as the other ceilings: a dashboard
                    // wants the limit before any network workload exists.
                    runtime.network_ceiling_ops_per_second = max_network_ops_per_second;
                    runtime.thp = crate::state::ThpRuntime {
                        requested: arena.thp_policy().as_str().to_owned(),
                        host_enabled: matches!(thp_host.as_str(), "always" | "madvise"),
                        host: thp_host.clone(),
                    };

                    match memory {
                        Ok(target_arena_bytes) => {
                            runtime.last_target_arena_bytes = target_arena_bytes as u64;
                            let error =
                                signed_delta(target_arena_bytes, runtime.arena_resident_bytes);
                            runtime.convergence_error_bytes = error;
                            runtime.converged =
                                error.unsigned_abs() <= config.controller.tolerance_bytes;
                        }
                        Err(error) => {
                            runtime.convergence_error_bytes = 0;
                            runtime.converged = false;
                            tick_error = Some(error.to_string());
                        }
                    }

                    if tick_error.is_some() {
                        runtime.reconcile_errors_total =
                            runtime.reconcile_errors_total.saturating_add(1);
                    }
                    runtime.last_error = tick_error;
                }

                let tick = Duration::from_millis(config.controller.tick_ms);
                // Saturating rather than checked: a tick that overran is a tick that sleeps
                // for no time at all, which is what `if elapsed < tick` was expressing.
                let sleep = tick.saturating_sub(tick_start.elapsed());
                if sleep.is_zero() {
                    continue;
                }
                // Sleep in slices so shutdown does not have to wait out a full tick. A controller
                // holding a file-backed mapping has to be able to unlink it promptly, and `main`
                // joins this thread under a grace period that is shorter than the largest
                // permitted `tick_ms`. Sleeping the whole remainder once would leave the file on
                // disk for any tick longer than that grace.
                sleep_interruptibly(sleep, &shutdown);
            }
        })
        .expect("failed to spawn loadlab controller thread")
}

/// Sleep for at most `total`, returning early once `shutdown` is set.
///
/// Checked in [`SHUTDOWN_SLICE`] slices rather than once at the end. The granularity is a
/// compromise: it bounds how long a clean shutdown can be delayed by to one slice, and it costs one
/// atomic load per slice, which is nothing next to the reconciliation work a tick does.
fn sleep_interruptibly(total: Duration, shutdown: &AtomicBool) {
    let mut left = total;
    while !left.is_zero() {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let slice = left.min(SHUTDOWN_SLICE);
        thread::sleep(slice);
        left -= slice;
    }
}

/// The longest a shutdown signal can go unnoticed while the controller is sleeping.
const SHUTDOWN_SLICE: Duration = Duration::from_millis(20);

/// The wall-clock and CPU-time pair one tick's utilization is measured over.
struct Measurement {
    cpu_seconds: f64,
    wall: Instant,
}

/// Which workers a pool was built for.
///
/// Compared every tick against the current configuration. A difference means the pool's
/// thread count or placement no longer matches what was asked for, which cannot be fixed
/// in place, so the pool is torn down and rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PoolSpec {
    workers: usize,
    cpus: Vec<usize>,
    affinity: Affinity,
}

/// Which access controller configuration a buffer was built for.
///
/// Compared every tick against the current configuration. A difference means the controller's
/// buffer size, pattern, or parameters no longer match what was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AccessSpec {
    pattern: crate::state::AccessPattern,
    working_set_pages: Option<usize>,
    stride_pages: Option<usize>,
    random_seed: Option<u64>,
}

/// Build the `AccessSpec` a configuration calls for.
fn access_spec(config: &WorkloadState) -> AccessSpec {
    AccessSpec {
        pattern: config.memory_access.pattern.clone(),
        working_set_pages: config.memory_access.working_set_pages,
        stride_pages: config.memory_access.stride_pages,
        random_seed: effective_seed(
            config,
            config.memory_access.random_seed,
            crate::experiment::Stream::MemoryAccess,
        ),
    }
}

/// The seed a controller should actually use.
///
/// The experiment seed is the fallback and a controller's own `random_seed` is the override, so a
/// scenario that set one explicitly keeps the walk it always had, and one that set neither becomes
/// reproducible from a single number.
///
/// Resolved on every tick rather than once, because the three call sites compare their settings
/// against the live controller and rebuild when they differ. That is also what makes a seed change
/// at runtime take effect immediately instead of at the next restart.
fn effective_seed(
    config: &WorkloadState,
    controller_seed: Option<u64>,
    stream: crate::experiment::Stream,
) -> Option<u64> {
    crate::experiment::stream_seed(config.experiment.seed, controller_seed, stream)
}

/// Build the `PoolSpec` a configuration calls for.
fn pool_spec(config: &WorkloadState) -> Option<PoolSpec> {
    // `validate` has already run on this state, so the affinity parses and the CPU
    // indices are in range. A parse failure here would mean the state was replaced
    // without going through validation, and guessing is worse than doing nothing.
    Affinity::parse(&config.cpu.affinity)
        .ok()
        .map(|affinity| PoolSpec {
            workers: config.cpu.workers,
            cpus: config.cpu.cpus.clone(),
            affinity,
        })
}

/// Reconcile the CPU pool against the configuration and return what it is doing.
fn reconcile_cpu(
    config: &WorkloadState,
    elapsed_seconds: f64,
    pool: &mut Option<CpuPool>,
    spec: &mut Option<PoolSpec>,
    duty: &mut DutyController,
    expressions: &mut ExpressionCache,
    measurement: &mut Option<Measurement>,
) -> Result<CpuRuntime, LoadlabError> {
    // Measure before changing anything, so this reading describes the window the pool
    // actually ran under -- that is, the previous tick's target, not this one's.
    let now = procfs::cpu_time().total_seconds;
    let wall = Instant::now();
    let window_seconds = measurement.as_ref().map_or(0.0, |previous| {
        wall.duration_since(previous.wall).as_secs_f64()
    });
    let actual_pct = if window_seconds <= 0.0 {
        0.0
    } else {
        let cpu_seconds = (now
            - measurement
                .as_ref()
                .map_or(now, |previous| previous.cpu_seconds))
        .max(0.0);
        // Percent of a *single* logical CPU, which is the same unit as `cpu.utilization`.
        // Dividing by the core count here would make a reported 100 mean "the whole machine"
        // while a target of 100 means "one saturated CPU" -- two different numbers behind one
        // name, and the controller would then chase a target it could never reach.
        cpu_seconds / window_seconds * 100.0
    };
    *measurement = Some(Measurement {
        cpu_seconds: now,
        wall,
    });

    if window_seconds > 0.0 {
        // How finely this reading can resolve a target, in points of a core. The controller
        // needs it to know when a target is below the measurement floor and the integral
        // should stand aside rather than chase quantisation noise.
        duty.set_resolution_pct(procfs::clock_tick_seconds() / window_seconds * 100.0);
    }

    // Nothing is requested while the workload is stopped, whatever the expression would
    // evaluate to. Evaluating it anyway would make `/metrics` report a live-looking target
    // for a workload that is not running.
    let requested_pct = if config.running {
        evaluate_cpu(config, elapsed_seconds, expressions)?
    } else {
        0.0
    };

    // A target of zero has no work for a thread to do. Spawning a worker per logical CPU to
    // sit at duty zero would cost the threads and report `worker_count` as the machine's core
    // count for a workload that explicitly asked for none -- which is also what a default
    // `cpu.utilization` of "0" would otherwise produce the moment anything was started.
    //
    // The runtime is still filled in from the measurement rather than left blank, so a
    // stopped process reports itself converged once it is quiet. A dashboard showing "not
    // converged" for a process that is correctly doing nothing is the one state where an
    // operator most wants a clean signal.
    if requested_pct <= 0.0 {
        stop_pool(pool, spec);
        duty.reset();
        let demand = duty.update(actual_pct, 0.0, 0.0);
        return Ok(CpuRuntime {
            worker_count: 0,
            capacity_pct: 0.0,
            requested_utilization_pct: 0.0,
            target_utilization_pct: 0.0,
            actual_utilization_pct: demand.actual_pct,
            duty_cycle: 0.0,
            converged: demand.converged,
            saturated: false,
            // No workers means nothing to read. `scheduling_available` is false because there is no
            // observation, not because the kernel lacks the counters, and no dashboard should chart
            // a run-queue wait for a pool that does not exist.
            scheduling_available: false,
            worker_runqueue_wait_nanoseconds: 0,
            worker_worst_runqueue_wait_nanoseconds: 0,
        });
    }

    let requested_spec = pool_spec(config).ok_or_else(|| {
        LoadlabError::InvalidConfig(format!(
            "cpu.affinity {:?} is not a valid mode",
            config.cpu.affinity
        ))
    })?;
    if spec.as_ref() != Some(&requested_spec) {
        stop_pool(pool, spec);
        // The integral has to go with the old pool. It is a correction learned against one
        // particular arrangement of workers, and how much duty buys a given amount of CPU
        // changes the moment the worker count or placement does. Carrying it across a
        // rebuild makes every reconfiguration overshoot for as long as it takes to decay.
        duty.reset();
        let mut rebuilt = CpuPool::new(
            requested_spec.workers,
            &requested_spec.cpus,
            requested_spec.affinity,
        )?;
        rebuilt.spawn();
        *pool = Some(rebuilt);
        *spec = Some(requested_spec);
    }

    let Some(pool) = pool.as_ref() else {
        return Ok(CpuRuntime::default());
    };

    let demand = duty.update(actual_pct, requested_pct, pool.capacity_pct());
    pool.set_duty_cycle(demand.duty_cycle);

    // Sampled after the duty cycle is set, so the wait covers the interval the controller just asked
    // for. That is the ordering that makes the figure answer a question: if this tick's target was
    // missed and the workers spent the window runnable, the miss was the scheduler's.
    let scheduling = observation::worker_scheduling(&pool.worker_thread_ids());

    Ok(CpuRuntime {
        worker_count: pool.worker_count(),
        capacity_pct: demand.capacity_pct,
        requested_utilization_pct: requested_pct,
        target_utilization_pct: demand.target_pct,
        actual_utilization_pct: demand.actual_pct,
        duty_cycle: demand.duty_cycle,
        converged: demand.converged,
        saturated: demand.saturated,
        scheduling_available: scheduling.available,
        worker_runqueue_wait_nanoseconds: scheduling.runqueue_wait_nanoseconds,
        worker_worst_runqueue_wait_nanoseconds: scheduling.worst_runqueue_wait_nanoseconds,
    })
}

/// Stop the pool and forget which one it was.
fn stop_pool(pool: &mut Option<CpuPool>, spec: &mut Option<PoolSpec>) {
    if let Some(mut running) = pool.take() {
        running.stop();
    }
    *spec = None;
}

/// Evaluate `cpu.utilization` for this tick, reusing the cache.
///
/// A negative result is clamped rather than refused: an expression that dips below zero is a
/// normal thing to write, and the memory controller does the same. What must not happen is the
/// controller reporting a negative duty cycle.
fn evaluate_cpu(
    config: &WorkloadState,
    elapsed_seconds: f64,
    expressions: &mut ExpressionCache,
) -> Result<f64, LoadlabError> {
    Ok(expressions
        .evaluate(
            &config.cpu.utilization,
            Dimension::Unitless,
            elapsed_seconds,
        )?
        .max(0.0))
}

fn reconcile_memory(
    config: &WorkloadState,
    baseline_rss_anon_bytes: u64,
    elapsed_seconds: f64,
    arena: &mut AnonymousArena,
    expressions: &mut ExpressionCache,
) -> Result<usize, LoadlabError> {
    // Applied before the running check: the policy describes how the process pages memory, so it
    // holds whether or not an anonymous target is currently generating any. `thp_policy` cannot
    // fail here because `validate` rejected an unknown name before the state was accepted.
    arena.set_thp_policy(config.memory.thp_policy()?)?;

    let Some(setting) = &config.memory.anonymous_rss else {
        arena.set_resident_bytes(0)?;
        return Ok(0);
    };
    if !config.running {
        arena.set_resident_bytes(0)?;
        return Ok(0);
    }

    let desired_total_rss_anon = expressions
        .evaluate(setting, Dimension::Bytes, elapsed_seconds)?
        .max(0.0) as u64;
    let desired_arena = desired_total_rss_anon
        .saturating_sub(baseline_rss_anon_bytes)
        .min(config.controller.max_memory_bytes);
    let tolerance = config.controller.tolerance_bytes as usize;
    let actual = arena.resident_bytes();

    if actual.abs_diff(desired_arena as usize) > tolerance {
        arena.set_resident_bytes(desired_arena as usize)?;
    }

    Ok(desired_arena as usize)
}

fn signed_delta(target: usize, actual: u64) -> i64 {
    let target = i64::try_from(target).unwrap_or(i64::MAX);
    let actual = i64::try_from(actual).unwrap_or(i64::MAX);
    target.saturating_sub(actual)
}

/// A byte-valued target evaluated for this tick, or zero when absent.
fn evaluate_optional_bytes(
    setting: Option<&crate::setting::Setting>,
    elapsed_seconds: f64,
    expressions: &mut ExpressionCache,
) -> Result<u64, LoadlabError> {
    let Some(setting) = setting else {
        return Ok(0);
    };
    // Saturating rather than checked, for the same reason as the anonymous and file-backed
    // targets: `evaluate` has rejected negatives, and an expression that reaches 2^64 is far past
    // the operator ceiling that will clamp it anyway.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(expressions
        .evaluate(setting, Dimension::Bytes, elapsed_seconds)?
        .max(0.0) as u64)
}

/// Reconcile the shared memory and copy-on-write controller against the configuration.
///
/// Both targets are zero while stopped, so a paused experiment reports no segment and no actors
/// rather than a live-looking target. That matters more here than for the other controllers: an
/// actor is a process, so "stopped" has to mean the processes are gone rather than idle.
fn reconcile_shared(
    config: &WorkloadState,
    elapsed_seconds: f64,
    max_shared_bytes: u64,
    controller: &mut crate::shared::SharedController,
    expressions: &mut ExpressionCache,
) -> Result<crate::state::SharedRuntime, LoadlabError> {
    let faults = procfs::fault_stats()?;

    let shared_bytes = if config.running {
        evaluate_optional_bytes(
            config.shared.shared_bytes.as_ref(),
            elapsed_seconds,
            expressions,
        )?
    } else {
        0
    };
    let private_bytes = if config.running {
        evaluate_optional_bytes(
            config.shared.private_bytes.as_ref(),
            elapsed_seconds,
            expressions,
        )?
    } else {
        0
    };

    let cow_write_ratio = if config.running {
        expressions
            .evaluate(
                &config.shared.cow_write_ratio,
                Dimension::Unitless,
                elapsed_seconds,
            )?
            .clamp(0.0, 1.0)
    } else {
        0.0
    };

    let settings = crate::shared::SharedSettings {
        backing: config.shared.shared_backing(),
        shared_bytes,
        private_bytes_per_actor: private_bytes,
        cow_write_ratio,
        actors: config.shared.actors,
    };

    let snapshot = controller.reconcile(
        &settings,
        faults.minor_faults,
        config.controller.tolerance_bytes,
        max_shared_bytes,
    )?;

    Ok(crate::state::SharedRuntime {
        backing: snapshot.backing,
        shared_bytes: snapshot.shared_bytes,
        private_bytes_per_actor: snapshot.private_bytes_per_actor,
        cow_write_ratio: snapshot.cow_write_ratio,
        actors: snapshot.actors,
        child_actors: snapshot.child_actors,
        shared_physical_bytes: snapshot.shared_physical_bytes,
        shared_rss_bytes: snapshot.shared_rss_bytes,
        cow_physical_bytes: snapshot.cow_physical_bytes,
        accounted_bytes: snapshot.accounted_bytes,
        physical_bytes: snapshot.physical_bytes,
        shared_overcount_bytes: snapshot.shared_overcount_bytes,
        requested_cow_bytes: snapshot.requested_cow_bytes,
        target_cow_bytes: snapshot.target_cow_bytes,
        converged: snapshot.converged,
        saturated: snapshot.saturated,
        actor_accounting: snapshot
            .actor_accounting
            .iter()
            .map(|entry| crate::state::ActorRuntime {
                pid: entry.pid,
                is_controller: entry.is_controller,
                rss_bytes: entry.rss_bytes,
                pss_bytes: entry.pss_bytes,
                rss_shmem_bytes: entry.rss_shmem_bytes,
                rss_anon_bytes: entry.rss_anon_bytes,
                minor_faults: entry.minor_faults,
            })
            .collect(),
    })
}

/// Reconcile the memory-access controller against the configuration.
fn reconcile_access(
    config: &WorkloadState,
    elapsed_seconds: f64,
    access: &mut Option<AccessController>,
    spec: &mut Option<AccessSpec>,
    controller_tid: u32,
    expressions: &mut ExpressionCache,
) -> Result<crate::state::MemoryAccessRuntime, LoadlabError> {
    let faults = procfs::fault_stats()?;
    let target = if config.running {
        expressions
            .evaluate(
                &config.memory_access.pages_per_second,
                Dimension::Unitless,
                elapsed_seconds,
            )?
            .max(0.0)
    } else {
        0.0
    };

    let requested = access_spec(config);
    if *spec != Some(requested.clone()) {
        let req = requested.clone();
        *access = Some(AccessController::new(
            req.pattern,
            req.working_set_pages,
            req.stride_pages,
            req.random_seed,
        )?);
        *spec = Some(requested);
    }

    let controller = access.as_mut().expect("built above");
    let snapshot = controller.reconcile(faults.minor_faults, faults.major_faults, target);

    // The same process-wide counters the access controller just measured, split by thread so that
    // "faults this controller generated" can be told apart from faults the rest of the process took
    // while doing something else.
    let attribution = observation::fault_attribution(&faults, controller_tid);

    Ok(crate::state::MemoryAccessRuntime {
        pattern: config.memory_access.pattern.clone(),
        pages_per_second: target,
        actual_pages_per_second: snapshot.actual_minor_pps,
        // Taken from the attribution rather than from `faults` directly, so that the totals and the
        // split below are guaranteed to be one sample. Both would be the same numbers today; the
        // point is that nothing downstream can quietly re-read them apart.
        minor_faults_total: attribution.process_minor_faults,
        major_faults_total: attribution.process_major_faults,
        minor_faults_rate: snapshot.actual_minor_pps,
        major_faults_rate: snapshot.actual_major_pps,
        attribution_available: attribution.controller_found,
        generated_minor_faults: attribution.controller_minor_faults,
        generated_major_faults: attribution.controller_major_faults,
        incidental_minor_faults: attribution.other_minor_faults,
        incidental_major_faults: attribution.other_major_faults,
        converged: snapshot.converged,
        saturated: snapshot.saturated,
    })
}

/// Reconcile the file-backed memory controller against the configuration.
///
/// The controller is built lazily and then kept. That is deliberate in both directions: a
/// process that never asks for file-backed memory never creates a file, and a workload that
/// stops does not pay to recreate one, because the mapping is released with `MADV_DONTNEED`
/// rather than torn down. The file keeps its high-water length for the life of the process;
/// the operator ceiling is what bounds it.
fn reconcile_file_backed(
    config: &WorkloadState,
    elapsed_seconds: f64,
    max_file_backed_bytes: u64,
    file_backed_dir: &std::path::Path,
    controller: &mut Option<FileBackedController>,
    expressions: &mut ExpressionCache,
) -> Result<crate::state::FileBackedRuntime, LoadlabError> {
    let faults = procfs::fault_stats()?;

    // Both targets are zero while stopped, whatever the expressions would say, so a paused
    // experiment reports a quiet file-backed controller rather than a live-looking target.
    let target_rss_bytes = if config.running {
        match &config.file_backed.rss {
            Some(setting) => {
                // Same narrowing as the anonymous target above: a saturating cast, because
                // `evaluate` has already rejected negatives and an expression that reaches
                // 2^64 is far past the operator ceiling that will clamp it anyway.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let bytes = expressions
                    .evaluate(setting, Dimension::Bytes, elapsed_seconds)?
                    .max(0.0) as u64;
                bytes
            }
            None => 0,
        }
    } else {
        0
    };

    let target_pps = if config.running {
        expressions
            .evaluate(
                &config.file_backed.pages_per_second,
                Dimension::Unitless,
                elapsed_seconds,
            )?
            .max(0.0)
    } else {
        0.0
    };

    // Nothing asked for and nothing to release: report the quiet state without creating a
    // file. This is the common case, and it is what keeps a default workload from touching
    // the filesystem at all.
    if controller.is_none() && target_rss_bytes == 0 && target_pps == 0.0 {
        return Ok(quiet_file_backed());
    }

    if controller.is_none() {
        *controller = Some(FileBackedController::new(
            max_file_backed_bytes,
            file_backed_dir,
        )?);
    }
    let controller = controller.as_mut().expect("built above");

    // Pattern and readahead changes are applied in place. Nothing here needs a new file or a
    // new mapping, so unlike the CPU pool there is no rebuild to detect and no residency to
    // lose when the configuration moves.
    controller.configure(
        config.file_backed.access_pattern,
        config.file_backed.working_set_pages,
        config.file_backed.stride_pages,
        effective_seed(
            config,
            config.file_backed.random_seed,
            crate::experiment::Stream::FileBacked,
        ),
    );
    controller.apply_policy(config.file_backed.madvise_policy()?)?;

    let snapshot = controller.reconcile(
        faults.minor_faults,
        faults.major_faults,
        target_rss_bytes,
        target_pps,
        config.controller.tolerance_bytes,
    )?;

    Ok(crate::state::FileBackedRuntime {
        path: controller.path().to_owned(),
        file_bytes: snapshot.file_bytes,
        pages: snapshot.resident_pages,
        rss_bytes: snapshot.rss_bytes,
        requested_rss_bytes: snapshot.requested_rss_bytes,
        target_rss_bytes: snapshot.target_rss_bytes,
        process_rss_file_bytes: snapshot.process_rss_file_bytes,
        access_pattern: config.file_backed.access_pattern,
        madvise: config.file_backed.madvise.clone(),
        pages_per_second: target_pps,
        actual_pages_per_second: snapshot.actual_minor_pps,
        minor_faults_rate: snapshot.actual_minor_pps,
        major_faults_rate: snapshot.actual_major_pps,
        converged: snapshot.converged,
        saturated: snapshot.saturated,
    })
}

/// The reported state of a controller that has never been asked to do anything.
///
/// Empty path rather than a path with nothing behind it: no file exists, and publishing one
/// would invite an operator to look for it.
fn quiet_file_backed() -> crate::state::FileBackedRuntime {
    crate::state::FileBackedRuntime {
        converged: true,
        ..Default::default()
    }
}

/// Reconcile the disk I/O controller against the configuration.
///
/// The controller is built lazily and then kept, for the same reason as the file-backed one: a
/// process that never asks for I/O never creates a file, and a workload that stops does not pay to
/// recreate one. The region keeps its block-aligned size for the life of the process.
fn reconcile_io(
    config: &WorkloadState,
    elapsed_seconds: f64,
    max_io_bytes: u64,
    io_dir: &std::path::Path,
    controller: &mut Option<IoController>,
    expressions: &mut ExpressionCache,
    latency_histograms: &crate::metrics::IoLatencyHistograms,
) -> Result<crate::state::IoRuntime, LoadlabError> {
    // Only evaluated while running: a stopped workload reports no rate, not the expression's value
    // at the current `t`, and evaluating it would make `/metrics` show a live-looking target.
    let (read_bps, write_bps, iops, fsync_rate) = if config.running {
        (
            expressions
                .evaluate(&config.io.read_bps, Dimension::Bytes, elapsed_seconds)?
                .max(0.0),
            expressions
                .evaluate(&config.io.write_bps, Dimension::Bytes, elapsed_seconds)?
                .max(0.0),
            expressions
                .evaluate(&config.io.iops, Dimension::Unitless, elapsed_seconds)?
                .max(0.0),
            expressions
                .evaluate(&config.io.fsync_rate, Dimension::Unitless, elapsed_seconds)?
                .max(0.0),
        )
    } else {
        (0.0, 0.0, 0.0, 0.0)
    };

    let mut settings = config
        .io
        .settings(config.running, read_bps, write_bps, iops, fsync_rate);
    settings.random_seed =
        effective_seed(config, settings.random_seed, crate::experiment::Stream::Io);
    let quiet = settings.read_bps == 0.0
        && settings.write_bps == 0.0
        && settings.iops == 0.0
        && settings.fsync_rate == 0.0;

    // Nothing asked for: report the quiet state without creating a file. This is the common case
    // for every workload that does not use I/O, and it is what keeps a default workload from
    // touching the filesystem at all.
    if controller.is_none() && quiet {
        return Ok(quiet_io());
    }

    if controller.is_none() {
        *controller = Some(IoController::new(
            max_io_bytes,
            io_dir,
            latency_histograms.clone(),
        )?);
    }
    let controller = controller.as_mut().expect("built above");

    let snapshot = controller.reconcile(&settings)?;

    Ok(crate::state::IoRuntime {
        path: controller.path().to_owned(),
        region_bytes: controller.region_bytes(),
        block_size_bytes: snapshot.block_size,
        direct: snapshot.direct,
        queue_depth: settings.queue_depth,
        pattern: settings.pattern.as_str().to_owned(),
        read_bps: snapshot.read_bps,
        read_bps_actual: snapshot.read_bps_actual,
        write_bps: snapshot.write_bps,
        write_bps_actual: snapshot.write_bps_actual,
        iops: snapshot.iops,
        iops_actual: snapshot.iops_actual,
        fsync_rate: snapshot.fsync_rate,
        fsyncs_per_second: snapshot.fsyncs_per_second,
        read_bytes_total: snapshot.read_bytes_total,
        write_bytes_total: snapshot.write_bytes_total,
        ops_total: snapshot.ops_total,
        fsyncs_total: snapshot.fsyncs_total,
        read_latency: snapshot.read_latency,
        write_latency: snapshot.write_latency,
        fsync_latency: snapshot.fsync_latency,
        converged: snapshot.converged,
        saturated: snapshot.saturated,
    })
}

/// The reported state of a controller that has never been asked to do anything.
///
/// Empty path rather than a path with nothing behind it: no file exists, and publishing one would
/// invite an operator to look for it.
fn quiet_io() -> crate::state::IoRuntime {
    crate::state::IoRuntime {
        converged: true,
        ..Default::default()
    }
}

/// Reconcile the TCP/UDP generator against the configuration.
///
/// The controller is built lazily and then kept, for the same reason as the I/O one: a process that
/// never asks for network traffic never opens a socket, and a workload that stops drops its
/// connections rather than paying to rebuild them. The target is resolved on a change, on this
/// thread, so a slow resolver is never held under the state lock.
fn reconcile_network(
    config: &WorkloadState,
    elapsed_seconds: f64,
    max_network_ops_per_second: u64,
    controller: &mut Option<NetworkController>,
    expressions: &mut ExpressionCache,
) -> Result<crate::state::NetworkRuntime, LoadlabError> {
    // Evaluate only while running: a stopped workload reports no rate, not the expression's value
    // at the current `t`, for the same reason the CPU controller does. A negative result is clamped
    // to zero, like every other rate: a sinusoid that dips below zero is normal to write.
    let (connections_per_second, messages_per_second) = if config.running {
        (
            expressions
                .evaluate(
                    &config.network.connections_per_second,
                    Dimension::Unitless,
                    elapsed_seconds,
                )?
                .max(0.0),
            expressions
                .evaluate(
                    &config.network.messages_per_second,
                    Dimension::Unitless,
                    elapsed_seconds,
                )?
                .max(0.0),
        )
    } else {
        (0.0, 0.0)
    };
    let settings =
        config
            .network
            .settings(config.running, connections_per_second, messages_per_second);

    // Nothing targeted: report the quiet state without resolving or opening anything. This is the
    // common case for every workload that does not use the network.
    if controller.is_none() && settings.target.is_none() {
        return Ok(quiet_network());
    }

    if controller.is_none() {
        *controller = Some(NetworkController::new(max_network_ops_per_second));
    }
    let controller = controller.as_mut().expect("built above");

    let snapshot = controller.reconcile(&settings)?;

    Ok(crate::state::NetworkRuntime {
        transport: snapshot.transport,
        target: snapshot.target,
        concurrency: snapshot.concurrency,
        message_size_bytes: snapshot.message_size,
        connections_per_second: snapshot.connections_per_second,
        connections_per_second_actual: snapshot.connections_per_second_actual,
        messages_per_second: snapshot.messages_per_second,
        messages_per_second_actual: snapshot.messages_per_second_actual,
        bytes_per_second_actual: snapshot.bytes_per_second_actual,
        messages_total: snapshot.messages_total,
        bytes_total: snapshot.bytes_total,
        connections_total: snapshot.connections_total,
        errors_total: snapshot.errors_total,
        converged: snapshot.converged,
        saturated: snapshot.saturated,
    })
}

/// The reported state of a generator that has never been asked to do anything.
fn quiet_network() -> crate::state::NetworkRuntime {
    crate::state::NetworkRuntime {
        converged: true,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AtomicBool, Measurement, Ordering, SHUTDOWN_SLICE, evaluate_cpu, pool_spec, reconcile_cpu,
        sleep_interruptibly,
    };
    use crate::{
        cpu::{Affinity, CpuPool, DutyController},
        setting::{ExpressionCache, Setting},
        state::WorkloadState,
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    /// The expression is compiled once and reused across ticks, because recompiling on every tick
    /// would put a parser in the control loop; changing the text recompiles it.
    #[test]
    fn the_cpu_expression_is_cached_across_ticks_and_recompiled_on_change() {
        let mut config = WorkloadState::default();
        config.cpu.utilization = Setting::Expression("50 + 50*t/10".to_owned());
        let mut expressions = ExpressionCache::default();

        assert_eq!(
            evaluate_cpu(&config, 10.0, &mut expressions).expect("first"),
            100.0
        );
        assert_eq!(expressions.cached_len(), 1);
        assert_eq!(
            evaluate_cpu(&config, 20.0, &mut expressions).expect("second"),
            150.0
        );
        assert_eq!(
            expressions.cached_len(),
            1,
            "an unchanged expression was recompiled"
        );

        config.cpu.utilization = Setting::constant(10.0);
        assert_eq!(
            evaluate_cpu(&config, 0.0, &mut expressions).expect("changed"),
            10.0
        );
    }

    /// A CPU expression that fails at runtime is an error, not a silent zero. It cannot
    /// normally happen -- `validate` parses the same text -- but a controller that reports
    /// 0% of a healthy CPU because an expression broke is worse than one that says so.
    #[test]
    fn a_broken_cpu_expression_surfaces_as_an_error() {
        let mut config = WorkloadState::default();
        config.cpu.utilization = Setting::Expression("1MiB".to_owned());
        let mut expressions = ExpressionCache::default();
        let error = evaluate_cpu(&config, 0.0, &mut expressions).expect_err("byte units");
        assert!(error.to_string().contains("1MiB"), "{error}");
    }

    #[test]
    fn a_negative_target_is_clamped_to_zero() {
        let mut config = WorkloadState::default();
        config.cpu.utilization = Setting::Expression("10 - t".to_owned());
        let mut expressions = ExpressionCache::default();
        assert_eq!(
            evaluate_cpu(&config, 100.0, &mut expressions).expect("clamped"),
            0.0
        );
    }

    #[test]
    fn pool_spec_reflects_the_cpu_configuration() {
        let mut config = WorkloadState::default();
        config.cpu.workers = 3;
        config.cpu.cpus = vec![0, 1];
        config.cpu.affinity = "pin".to_owned();

        let spec = pool_spec(&config).expect("spec");
        assert_eq!(spec.workers, 3);
        assert_eq!(spec.cpus, vec![0, 1]);
        assert_eq!(spec.affinity, Affinity::Pin);

        // A configuration that would produce a different set of threads has to produce a
        // different spec, or the pool would never be rebuilt.
        let other = pool_spec(&config.clone()).expect("spec");
        assert_eq!(spec, other);
        config.cpu.workers = 4;
        assert_ne!(Some(spec), pool_spec(&config));
    }

    #[test]
    fn an_invalid_affinity_has_no_spec() {
        let mut config = WorkloadState::default();
        config.cpu.affinity = "sideways".to_owned();
        assert!(pool_spec(&config).is_none());
    }

    /// The first reading has no window to divide by, so it reports nothing rather than
    /// dividing by zero.
    #[test]
    fn the_first_measurement_has_no_window() {
        let previous: Option<Measurement> = None;
        let wall = Instant::now();
        assert_eq!(
            previous.map_or(0.0, |p| wall.duration_since(p.wall).as_secs_f64()),
            0.0
        );
    }

    /// Rebuilding on a worker-count change is the point of `PoolSpec`.
    #[test]
    fn a_pool_can_be_rebuilt_for_a_different_worker_count() {
        let mut first = CpuPool::new(1, &[], Affinity::Auto).expect("pool");
        first.spawn();
        first.stop();
        drop(first);

        let mut second = CpuPool::new(2, &[], Affinity::Auto).expect("pool");
        second.spawn();
        assert_eq!(second.worker_count(), 2);
        second.stop();
    }

    /// Releasing a pool happens inside the controller tick, on every workload stop. If it
    /// blocked, a stop would open a hole in the tick long enough to stall *memory*
    /// reconciliation too, for reasons that have nothing to do with memory. The threads
    /// actually leaving is covered in `cpu`, where it can be observed directly.
    #[test]
    fn releasing_a_running_pool_does_not_block_the_caller() {
        let start = Instant::now();
        {
            let mut pool = CpuPool::new(2, &[], Affinity::Auto).expect("pool");
            pool.spawn();
            pool.set_duty_cycle(1.0);
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "releasing a saturated pool took {:?}, which would stall the tick",
            start.elapsed()
        );
    }

    /// The default state is not running, so no pool is wanted and none is described.
    #[test]
    fn the_default_configuration_describes_a_valid_pool() {
        let state = WorkloadState::default();
        let spec = pool_spec(&state).expect("default affinity is valid");
        assert_eq!(spec.workers, 0);
        assert!(spec.cpus.is_empty());
        assert_eq!(spec.affinity, Affinity::Auto);
    }

    /// `workers: 0` means one worker per logical CPU, so a *running* workload with the
    /// default `cpu.utilization` of `"0"` would otherwise spawn a thread per core to sit at
    /// duty zero, and report `worker_count` as the machine's core count for a workload that
    /// asked for no CPU at all. The default is what any partial `PATCH` that never mentions
    /// CPU restores, so this is reachable by accident rather than by intent.
    #[test]
    fn a_zero_target_spawns_no_workers_even_while_running() {
        let mut config = WorkloadState::default();
        config.running = true;
        let mut pool = None;
        let mut spec = None;
        let mut duty = DutyController::new();
        let mut expressions = ExpressionCache::default();
        let mut measurement = None;

        let runtime = reconcile_cpu(
            &config,
            0.0,
            &mut pool,
            &mut spec,
            &mut duty,
            &mut expressions,
            &mut measurement,
        )
        .expect("reconcile");

        assert!(
            pool.is_none(),
            "no threads should exist for a target of zero"
        );
        assert_eq!(runtime.worker_count, 0);
        assert_eq!(runtime.capacity_pct, 0.0);
        assert_eq!(runtime.target_utilization_pct, 0.0);
        assert_eq!(runtime.duty_cycle, 0.0);
        assert!(!runtime.saturated);
    }

    /// A stopped workload reports nothing requested even when its expression would evaluate
    /// to something, otherwise `/metrics` would show a live-looking target for a process
    /// that is deliberately not running.
    #[test]
    fn a_stopped_workload_requests_nothing() {
        let mut config = WorkloadState::default();
        config.cpu.utilization = Setting::Expression("900".to_owned());
        let mut pool = None;
        let mut spec = None;
        let mut duty = DutyController::new();
        let mut expressions = ExpressionCache::default();
        let mut measurement = None;

        let runtime = reconcile_cpu(
            &config,
            0.0,
            &mut pool,
            &mut spec,
            &mut duty,
            &mut expressions,
            &mut measurement,
        )
        .expect("reconcile");

        assert_eq!(runtime.requested_utilization_pct, 0.0);
        assert_eq!(runtime.target_utilization_pct, 0.0);
        assert!(pool.is_none());
    }

    /// A nonzero target does build the pool, and a change to the worker count rebuilds it
    /// rather than leaving the original threads in place.
    #[test]
    fn a_nonzero_target_builds_a_pool_and_a_worker_change_replaces_it() {
        let mut config = WorkloadState::default();
        config.running = true;
        config.cpu.utilization = Setting::Expression("50".to_owned());
        config.cpu.workers = 1;
        let mut pool = None;
        let mut spec = None;
        let mut duty = DutyController::new();
        let mut expressions = ExpressionCache::default();
        let mut measurement = None;

        let first = reconcile_cpu(
            &config,
            0.0,
            &mut pool,
            &mut spec,
            &mut duty,
            &mut expressions,
            &mut measurement,
        )
        .expect("reconcile");
        assert_eq!(first.worker_count, 1);
        assert_eq!(first.capacity_pct, 100.0);
        assert_eq!(first.target_utilization_pct, 50.0);
        assert_eq!(spec, pool_spec(&config));

        config.cpu.workers = 2;
        let second = reconcile_cpu(
            &config,
            0.0,
            &mut pool,
            &mut spec,
            &mut duty,
            &mut expressions,
            &mut measurement,
        )
        .expect("reconcile");
        assert_eq!(second.worker_count, 2);
        assert_eq!(second.capacity_pct, 200.0);
        assert_eq!(
            second.target_utilization_pct, 50.0,
            "the target is a property of the workload, not of the thread count"
        );

        pool.as_mut().expect("pool").stop();
    }

    /// A sleep that is never interrupted is fine at the default tick, and quietly wrong at the
    /// largest one: `main` gives the join a five second grace, so a ten second tick would always
    /// time out and leave the file-backed file on disk. This pins the early return directly rather
    /// than hoping a timing test catches it.
    #[test]
    fn a_sleep_gives_up_as_soon_as_shutdown_is_set() {
        let shutdown = AtomicBool::new(false);
        let start = Instant::now();
        sleep_interruptibly(SHUTDOWN_SLICE, &shutdown);
        // Never set, so the sleep ran to completion: this is the branch where no early return
        // happens, and it is what the controller does on every tick.
        assert!(start.elapsed() >= SHUTDOWN_SLICE);

        let shutdown = AtomicBool::new(true);
        let start = Instant::now();
        sleep_interruptibly(Duration::from_secs(30), &shutdown);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "an already-set flag must return without sleeping, took {:?}",
            start.elapsed()
        );
    }

    /// The property that actually matters: a flag set shortly into a long sleep cuts the wait
    /// down to roughly one slice rather than the full duration.
    #[test]
    fn a_long_sleep_is_cut_short_by_a_flag_set_part_way_through() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let setter = std::thread::spawn(move || {
            std::thread::sleep(SHUTDOWN_SLICE * 3);
            flag.store(true, Ordering::Relaxed);
        });
        let start = Instant::now();
        sleep_interruptibly(Duration::from_secs(60), &shutdown);
        let elapsed = start.elapsed();
        setter.join().expect("setter");
        assert!(
            elapsed < Duration::from_secs(5),
            "expected an early return, waited {elapsed:?}"
        );
    }
}
