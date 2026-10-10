//! loadlab: a programmable Linux workload laboratory.
//!
//! Users express a desired resource workload as state, mutate that state over HTTP while the
//! process runs, and let a controller converge toward it while `/proc` reports what the kernel
//! actually did.

mod api;
mod controller;
mod cpu;
mod error;
mod experiment;
mod expression;
mod file_backed;
mod io;
mod memory;
mod memory_access;
mod metrics;
mod network;
mod observation;
mod procfs;
mod security;
mod setting;
mod shared;
mod state;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use axum::serve;
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::{
    api::{AppState, Limits},
    state::{ExperimentConfig, RuntimeStatus, WorkloadState},
};

#[derive(Debug, Parser)]
#[command(
    name = "loadlab",
    version,
    about = "Programmable Linux workload laboratory"
)]
struct Args {
    /// HTTP bind address. Keep this on loopback unless remote control is explicitly desired.
    #[arg(long, env = "LOADLAB_BIND", default_value = "127.0.0.1:8080")]
    bind: SocketAddr,

    /// Hard ceiling on generated anonymous memory in bytes.
    ///
    /// This is a process-level safety limit. Requests may lower `controller.max_memory_bytes`
    /// at runtime but can never raise it above this value.
    #[arg(long, env = "LOADLAB_MAX_MEMORY_BYTES", default_value_t = 8 * 1024 * 1024 * 1024u64)]
    max_memory_bytes: u64,

    /// Hard ceiling on the file the file-backed memory controller may create, in bytes.
    ///
    /// Separate from `--max-memory-bytes` and much smaller, because file-backed memory costs
    /// disk as well as RAM: every page loadlab pages in is a page the filesystem has allocated
    /// a block for. A request above this is clamped and reported as saturated rather than
    /// refused, the same way an above-capacity CPU target is.
    #[arg(
        long,
        env = "LOADLAB_MAX_FILE_BACKED_BYTES",
        default_value_t = 1024 * 1024 * 1024u64
    )]
    max_file_backed_bytes: u64,

    /// Directory the file-backed memory controller creates its temporary file in.
    ///
    /// The file name is always chosen by loadlab and unlinked on exit, so nothing in this
    /// directory can be overwritten. Defaults to the current working directory rather than the
    /// system temporary directory, because `/tmp` is usually tmpfs and a file there is accounted
    /// as shared memory instead of file memory — which would make the workload silently not do
    /// what was asked. Startup fails if the directory turns out to be memory-backed.
    #[arg(long, env = "LOADLAB_FILE_BACKED_DIR")]
    file_backed_dir: Option<std::path::PathBuf>,

    /// Hard ceiling on shared plus copy-on-write memory, in bytes.
    ///
    /// Bounds the *combined* footprint, which is what makes it a separate ceiling rather than
    /// another field: the shared segment is one copy no matter how many actors map it, but the
    /// copy-on-write pages are `private_bytes * actors`, so the total grows with the actor count
    /// while neither requested field does.
    #[arg(long, env = "LOADLAB_MAX_SHARED_BYTES", default_value_t = 2 * 1024 * 1024 * 1024u64)]
    max_shared_bytes: u64,

    /// Hard ceiling on the temporary file the disk I/O controller may create, in bytes.
    ///
    /// The I/O region is a real file, so this bounds disk usage independently of the memory
    /// ceilings. It is rounded up to a whole block, so the region can exceed it by less than one
    /// block; the effective size is reported as `io_ceiling_bytes`.
    #[arg(long, env = "LOADLAB_MAX_IO_BYTES", default_value_t = 1024 * 1024 * 1024u64)]
    max_io_bytes: u64,

    /// Hard ceiling on network messages plus fresh connections per second.
    ///
    /// Unlike the memory and disk ceilings, this bounds something that leaves the host, so it is
    /// the one safety limit that protects systems other than this one. A request above it is
    /// clamped and reported as saturated, the same way an above-capacity CPU target is. The
    /// effective value is reported as `network_ceiling_ops_per_second`.
    #[arg(
        long,
        env = "LOADLAB_MAX_NETWORK_OPS_PER_SECOND",
        default_value_t = 100_000u64
    )]
    max_network_ops_per_second: u64,

    /// Bearer token the HTTP API requires, if any.
    ///
    /// Optional by design. loadlab binds loopback by default, so on the machine it is meant to run
    /// on there is nothing to authenticate and requiring a secret would only be something to get
    /// wrong. Setting this is what makes the API safe to expose beyond loopback, and it is the
    /// difference between "anyone who can reach the port" and "whoever holds the token".
    ///
    /// Sent as `Authorization: Bearer <token>`. A blank value is treated as unset, because that is
    /// what an empty environment variable looks like. Prefer `LOADLAB_API_TOKEN` or a file read
    /// over `--api-token`, since a command line is visible in `ps` to every user on the host.
    #[arg(long, env = "LOADLAB_API_TOKEN")]
    api_token: Option<String>,

    /// Seed for the whole workload, for a repeatable run.
    ///
    /// The fallback for every controller that does not set its own `random_seed`, and the reason
    /// one number can identify a run: same seed, same workload and same start time walks the same
    /// memory access pattern, file-backed pattern and I/O path. Left unset, each controller keeps
    /// its own default and the run is not reproducible.
    #[arg(long, env = "LOADLAB_EXPERIMENT_SEED")]
    seed: Option<u64>,

    /// RFC 3339 instant to treat as `t = 0`, instead of the moment the workload started.
    ///
    /// The point of a stable time base: a scenario can carry expressions written against a shared
    /// timeline, so two hosts running the same scenario at different moments agree on what `t` is.
    /// Without it, `t` restarts whenever the workload does, and a restart partway through a
    /// scenario no longer means the same thing.
    ///
    /// A timestamp in the future is accepted, and `t` is negative until it arrives.
    #[arg(long, env = "LOADLAB_EXPERIMENT_START_TIME")]
    start_time: Option<String>,

    /// Largest HTTP request body accepted, in bytes.
    ///
    /// Every body this API takes is a workload description a few kilobytes long, so the default is
    /// far above anything legitimate. It bounds what an unauthenticated caller can make the process
    /// buffer, which on this machine would compete with the workload being measured.
    #[arg(long, env = "LOADLAB_MAX_BODY_BYTES", default_value_t = security::MAX_BODY_BYTES)]
    max_body_bytes: usize,

    /// Mutations per second the API accepts before refusing with `429`.
    ///
    /// Applies to `PUT`, `PATCH`, `POST` and `DELETE`; reads are not limited, because a scrape in a
    /// loop is a legitimate thing to do and does not change state. A request over the limit is
    /// refused with `Retry-After` rather than queued, since queueing would mean accepting the work
    /// and only delaying the answer.
    #[arg(
        long,
        env = "LOADLAB_MUTATION_RATE_PER_SECOND",
        default_value_t = security::MUTATION_RATE_PER_SECOND
    )]
    mutation_rate_per_second: f64,

    /// Mutations that may be spent at once before the rate limit starts refusing.
    ///
    /// This is what an interactive session draws down and what a paused client finds waiting for it.
    /// Two seconds' worth of mutations by default, so a script that reconfigures a handful of times
    /// in a row is never refused.
    #[arg(
        long,
        env = "LOADLAB_MUTATION_BURST",
        default_value_t = security::MUTATION_BURST
    )]
    mutation_burst: f64,

    /// Run a discarding TCP and UDP sink on this address instead of the controller and HTTP API.
    ///
    /// This is loadlab's blackhole: it accepts TCP connections and discards their bytes, and
    /// receives and discards UDP datagrams, so a network workload has somewhere to point without an
    /// external server. It binds both protocols on the same address.
    #[arg(long, env = "LOADLAB_SINK")]
    sink: Option<SocketAddr>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Actor mode is dispatched before anything else, including argument parsing: an actor is a
    // different program shape, with no runtime, no logging subscriber and no server, and its flags
    // deliberately do not appear in `Args` at all. They are parsed by hand here so that the parent,
    // which writes `--actor-fd=<n>` strings, and the child, which reads exactly those strings,
    // cannot drift, and so none of them appear in `--help` where an operator would be invited to
    // set them. Doing this after `Args::parse` would be too late: clap rejects the unknown `--actor`
    // flag before the child ever reaches its own code.
    if let Some(actor_args) = crate::shared::ActorArgs::from_args(std::env::args_os()) {
        return crate::shared::run_actor(&actor_args).map_err(|error| error.to_string().into());
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("loadlab=info".parse()?))
        .init();

    let args = Args::parse();

    // Sink mode is a different program shape: no controller, no runtime work, no HTTP API. It is
    // handled before any of the workload setup so a blackhole can be run with nothing else present.
    if let Some(sink) = args.sink {
        info!(address = %sink, "loadlab network sink listening");
        tokio::select! {
            result = crate::network::run_sink(sink) => {
                result?;
            }
            _ = shutdown_signal() => {}
        }
        return Ok(());
    }

    if args.max_memory_bytes == 0 {
        return Err("LOADLAB_MAX_MEMORY_BYTES must be greater than zero".into());
    }
    if args.max_file_backed_bytes == 0 {
        return Err("LOADLAB_MAX_FILE_BACKED_BYTES must be greater than zero".into());
    }
    if args.max_shared_bytes == 0 {
        return Err("LOADLAB_MAX_SHARED_BYTES must be greater than zero".into());
    }
    if args.max_io_bytes == 0 {
        return Err("LOADLAB_MAX_IO_BYTES must be greater than zero".into());
    }
    if args.max_network_ops_per_second == 0 {
        return Err("LOADLAB_MAX_NETWORK_OPS_PER_SECOND must be greater than zero".into());
    }

    // The controller narrows byte targets to `usize`. On a 32-bit host an operator ceiling wider
    // than the address space would make every subsequent request fail validation, so clamp it
    // here and report the effective value.
    let max_memory_bytes = args.max_memory_bytes.min(usize::MAX as u64);
    if max_memory_bytes < args.max_memory_bytes {
        warn!(
            requested = args.max_memory_bytes,
            effective = max_memory_bytes,
            "clamped max-memory-bytes to the addressable limit of this platform"
        );
    }

    let mut initial = WorkloadState::default();
    initial.controller.max_memory_bytes = max_memory_bytes;
    // Set here so a run started from the command line is as reproducible as one installed through
    // the API, and so the value is visible in `GET /state` from the first request rather than only
    // in the controller's own memory.
    initial.experiment = ExperimentConfig {
        seed: args.seed,
        start_time: args.start_time,
    };
    initial.normalize();
    initial.validate()?;

    let baseline = crate::procfs::memory_stats()?.rss_anon_bytes;
    // Read once, before serving. A failure to describe the host is a startup failure rather than a
    // degraded scrape: host metadata is what makes a measurement comparable to the next one, and a
    // measurement without it is one nobody can reason about later.
    let host = crate::procfs::host_info()?;
    let workload = Arc::new(RwLock::new(initial));
    let runtime = Arc::new(RwLock::new(RuntimeStatus::new(baseline)));
    // Built before serving so a duplicate metric name fails the process at startup rather than
    // producing malformed scrape output later.
    let metrics = Arc::new(crate::metrics::Metrics::new(&host)?);
    let app_state = AppState {
        workload: Arc::clone(&workload),
        runtime: Arc::clone(&runtime),
        limits: Limits { max_memory_bytes },
        host: host.clone(),
        metrics: Arc::clone(&metrics),
        scenarios: Arc::new(RwLock::new(HashMap::new())),
        active_scenario: Arc::new(RwLock::new(None)),
    };

    let file_backed_dir = args
        .file_backed_dir
        .unwrap_or_else(crate::file_backed::default_directory);
    // Checked here rather than on first use so a bad directory is a startup failure the operator
    // sees immediately, not a per-tick error repeating for as long as the workload is switched
    // on. The controller re-checks when it actually creates the file.
    crate::file_backed::check_directory(&file_backed_dir)?;
    // Set once the HTTP server has stopped, so the controller thread can leave its loop and drop
    // what it owns. Relaxed is sufficient: the flag carries a request to stop, not data, and the
    // controller re-reads it every tick regardless of ordering.
    let shutdown = Arc::new(AtomicBool::new(false));
    let controller = controller::spawn(
        workload,
        runtime,
        args.max_file_backed_bytes,
        file_backed_dir.clone(),
        args.max_shared_bytes,
        args.max_io_bytes,
        file_backed_dir,
        args.max_network_ops_per_second,
        metrics.io_latency_histograms(),
        Arc::clone(&shutdown),
    );
    let security = security::Security::new(
        args.api_token,
        args.mutation_rate_per_second,
        args.mutation_burst,
    )
    .unwrap_or_else(|message| {
        eprintln!("loadlab: {message}");
        std::process::exit(2);
    });
    let app = api::router(app_state, security.clone(), args.max_body_bytes);

    let listener = TcpListener::bind(args.bind).await?;
    let address = listener.local_addr()?;
    if security.is_open() {
        // Stated at startup rather than only in the docs, because the two cases are not
        // distinguishable from the client side and the wrong one is a machine anyone can saturate.
        warn!(%address, "the HTTP API is open: no token configured, so anything that can reach \
                         this port controls the workload. Set --api-token before exposing it");
    }
    info!(address = %address, "loadlab HTTP API listening");

    // The peer address is attached to every request, which is what lets the query-string token
    // fallback confirm it arrived over loopback. Without it that fallback could never fire, and
    // would silently be dead code that looks like a working feature.
    serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    shutdown.store(true, Ordering::Relaxed);
    // The controller thread is detached, so it is not joined when the process exits and the
    // file-backed file it owns is never unlinked. That leaves the operator's disk full of
    // `.tmp*` files after any clean shutdown, which is exactly the wrong behaviour for a tool
    // that exists to write to disk. Joining here makes the teardown deterministic: the thread
    // leaves its loop, drops its mapping, and unlinks its file before the process returns.
    //
    // Wait for the thread, but not forever: the loop is only checked between ticks, and a
    // controller wedged in a syscall would otherwise hang the exit indefinitely. Exiting with the
    // file still on disk is a far better outcome than never exiting at all.
    let deadline = Instant::now() + SHUTDOWN_JOIN_GRACE;
    while !controller.is_finished() && Instant::now() < deadline {
        std::thread::sleep(SHUTDOWN_POLL_INTERVAL);
    }
    if controller.is_finished() {
        // A panicked thread is reported rather than propagated: the workload state is already
        // committed and the HTTP server has stopped, so there is nothing left to serve.
        if controller.join().is_err() {
            warn!("controller thread panicked");
        }
    } else {
        warn!(
            grace_ms = SHUTDOWN_JOIN_GRACE.as_millis(),
            "controller thread did not stop in time; its file-backed file may be left on disk"
        );
    }

    Ok(())
}

/// How long shutdown waits for the controller thread before giving up on it.
const SHUTDOWN_JOIN_GRACE: Duration = Duration::from_secs(5);

/// How often shutdown checks whether the controller thread has finished.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(20);

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
