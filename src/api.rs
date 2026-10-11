use std::sync::{Arc, RwLock};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::header,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::{
    error::LoadlabError,
    metrics, procfs,
    procfs::HostInfo,
    security::{Security, limit_body_size, limit_mutation_rate, require_token},
    state::{
        CpuRuntime, FileBackedRuntime, IoRuntime, MemoryAccessRuntime, NetworkRuntime,
        RuntimeStatus, SharedRuntime, ThpRuntime, WorkloadState,
    },
};

/// Process-level limits fixed by the operator at startup.
///
/// These are deliberately not part of `WorkloadState`: a safety ceiling is a property of the
/// process, not of the workload a client happens to be asking for. If it lived in the desired
/// state, `#[serde(default)]` would let any request that omits `controller` silently restore the
/// built-in default and lift the ceiling the operator set with `--max-memory-bytes`.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_memory_bytes: u64,
}

#[derive(Clone)]
pub struct AppState {
    pub workload: Arc<RwLock<WorkloadState>>,
    pub runtime: Arc<RwLock<RuntimeStatus>>,
    pub limits: Limits,
    /// The host this process is running on, read once at startup.
    ///
    /// Held rather than re-read per scrape because none of it changes while loadlab runs, and a
    /// measurement's meaning depends on it: two runs on different hosts are the same series and
    /// different events.
    pub host: HostInfo,
    /// Built once at startup and shared by every scrape. Metric names are validated for
    /// uniqueness here rather than on each request.
    pub metrics: Arc<metrics::Metrics>,
}

/// Build the API.
///
/// The layers are ordered so each one is the cheapest test for the request that follows it: the
/// body limit rejects an oversized body before a handler deserializes it, and authentication runs
/// before the rate limiter so an unauthenticated caller cannot spend a legitimate client's budget.
pub fn router(state: AppState, security: Security, max_body_bytes: usize) -> Router {
    // `/health` is deliberately outside every layer. It is what a supervisor polls to know the
    // process came up, and it reports nothing an unauthenticated caller does not already know from
    // being able to connect at all.
    let health = Router::new().route("/health", get(health));

    let protected = Router::new()
        .route("/state", get(get_state))
        .route("/workload", put(put_workload).patch(patch_workload))
        .route("/workload/start", post(start))
        .route("/workload/stop", post(stop))
        .route("/metrics", get(metrics))
        .route("/prometheus", get(prometheus))
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(middleware::from_fn(move |request, next| {
            limit_body_size(max_body_bytes, request, next)
        }))
        .layer(middleware::from_fn_with_state(
            security.clone(),
            limit_mutation_rate,
        ))
        .layer(middleware::from_fn_with_state(security, require_token))
        .with_state(state);

    health.merge(protected)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn get_state(State(app): State<AppState>) -> Json<WorkloadState> {
    let value = read_lock(&app.workload).clone();
    Json(value)
}

async fn put_workload(
    State(app): State<AppState>,
    payload: Result<Json<WorkloadState>, JsonRejection>,
) -> Result<Json<WorkloadState>, LoadlabError> {
    let Json(mut value) = payload.map_err(json_rejection)?;
    apply_limits(&mut value, app.limits);
    value.validate()?;
    *write_lock(&app.workload) = value.clone();
    Ok(Json(value))
}

async fn patch_workload(
    State(app): State<AppState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Json<WorkloadState>, LoadlabError> {
    let Json(patch) = payload.map_err(json_rejection)?;

    // The merge is done under a single write lock so concurrent patches cannot interleave a
    // stale read with a later write. `validate` only parses and evaluates the expression, so no
    // syscall or filesystem access happens while the lock is held.
    let mut guard = write_lock(&app.workload);
    let mut value = serde_json::to_value(guard.clone())
        .map_err(|error| LoadlabError::Internal(error.to_string()))?;
    merge_patch(&mut value, &patch);
    let mut next: WorkloadState = serde_json::from_value(value)
        .map_err(|error| LoadlabError::InvalidConfig(error.to_string()))?;
    apply_limits(&mut next, app.limits);
    next.validate()?;
    *guard = next.clone();
    Ok(Json(next))
}

async fn start(State(app): State<AppState>) -> Json<WorkloadState> {
    let mut state = write_lock(&app.workload);
    state.running = true;
    Json(state.clone())
}

async fn stop(State(app): State<AppState>) -> Json<WorkloadState> {
    let mut state = write_lock(&app.workload);
    state.running = false;
    Json(state.clone())
}

async fn metrics(State(app): State<AppState>) -> Result<Json<MetricsResponse>, LoadlabError> {
    let memory = procfs::memory_stats()?;
    let faults = procfs::fault_stats()?;
    let cpu = procfs::cpu_stats()?;
    let workload = read_lock(&app.workload).clone();
    let runtime = read_lock(&app.runtime);

    Ok(Json(MetricsResponse {
        process: ProcessMetrics {
            memory,
            faults,
            cpu,
        },
        host: app.host.clone(),
        workload,
        controller: ControllerMetrics {
            elapsed_seconds: runtime.started_at.elapsed().as_secs_f64(),
            last_tick_age_seconds: runtime
                .last_tick_at
                .map(|instant| instant.elapsed().as_secs_f64()),
            tick_count: runtime.tick_count,
            arena_resident_bytes: runtime.arena_resident_bytes,
            baseline_rss_anon_bytes: runtime.baseline_rss_anon_bytes,
            last_target_arena_bytes: runtime.last_target_arena_bytes,
            convergence_error_bytes: runtime.convergence_error_bytes,
            converged: runtime.converged,
            reconcile_errors_total: runtime.reconcile_errors_total,
            last_error: runtime.last_error.clone(),
            cpu: runtime.cpu.clone(),
            memory_access: runtime.memory_access.clone(),
            file_backed: runtime.file_backed.clone(),
            shared: runtime.shared.clone(),
            io_ceiling_bytes: runtime.io_ceiling_bytes,
            io: runtime.io.clone(),
            network_ceiling_ops_per_second: runtime.network_ceiling_ops_per_second,
            network: runtime.network.clone(),
            thp: runtime.thp.clone(),
        },
    }))
}

/// Prometheus scrape target. Sits beside `/metrics` rather than replacing it: that endpoint is
/// JSON and is what the docs and tooling read, while a scraper needs the text format.
///
/// Reading procfs on every scrape is not free -- `smaps_rollup` walks the page tables, which on a
/// 600 MiB arena is proportional to the footprint this tool deliberately creates. At a 15 s
/// interval that is noise; at 1 s it is measurable.
async fn prometheus(State(app): State<AppState>) -> Result<Response, LoadlabError> {
    let memory = procfs::memory_stats()?;
    let faults = procfs::fault_stats()?;
    let cpu = procfs::cpu_stats()?;
    let workload = read_lock(&app.workload).clone();
    let runtime = read_lock(&app.runtime);

    let body = app
        .metrics
        .render(&workload, &runtime, &memory, &faults, &cpu)
        .map_err(|error| LoadlabError::Internal(error.to_string()))?;
    Ok(([(header::CONTENT_TYPE, metrics::CONTENT_TYPE)], body).into_response())
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct ProcessMetrics {
    memory: procfs::MemoryStats,
    faults: procfs::FaultStats,
    cpu: procfs::CpuStats,
}

#[derive(Debug, Serialize)]
struct ControllerMetrics {
    elapsed_seconds: f64,
    last_tick_age_seconds: Option<f64>,
    tick_count: u64,
    arena_resident_bytes: u64,
    baseline_rss_anon_bytes: u64,
    last_target_arena_bytes: u64,
    convergence_error_bytes: i64,
    converged: bool,
    reconcile_errors_total: u64,
    last_error: Option<String>,
    /// The CPU controller's own view. Distinct from `process.cpu`, which is a cumulative
    /// reading of what the kernel has charged so far rather than what the controller is
    /// currently aiming for.
    cpu: CpuRuntime,
    /// The memory-access controller's own view.
    memory_access: MemoryAccessRuntime,
    /// The file-backed memory controller's own view. Distinct from
    /// `process.memory.rss_file_bytes`, which is the whole process and includes the
    /// executable's own text.
    file_backed: FileBackedRuntime,
    /// The shared-memory controller's own view, including the per-actor accounting that says
    /// which process holds what.
    shared: SharedRuntime,
    /// The ceiling the operator set on the I/O region, as applied. A process-level limit, so it
    /// lives here rather than in `workload`.
    io_ceiling_bytes: u64,
    /// The disk I/O controller's own view, with requested and measured rates side by side.
    io: IoRuntime,
    /// The ceiling the operator set on network messages plus connections per second, as applied.
    /// A process-level limit, so it lives here rather than in `workload`.
    network_ceiling_ops_per_second: u64,
    /// The TCP/UDP generator's own view, with requested and measured rates side by side.
    network: NetworkRuntime,
    /// The transparent-huge-page policy and the host setting it was requested against.
    thp: ThpRuntime,
}

#[derive(Debug, Serialize)]
struct MetricsResponse {
    process: ProcessMetrics,
    /// The host the numbers below were produced on.
    host: HostInfo,
    workload: WorkloadState,
    controller: ControllerMetrics,
}

/// Apply RFC 7396 JSON Merge Patch semantics to `target`.
///
/// A `null` member in `patch` removes the corresponding key. Any other non-object
/// value replaces `target` outright. Objects merge key by key, so unrelated fields
/// survive a partial update.
fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch_map) = patch else {
        *target = patch.clone();
        return;
    };

    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Some(target_map) = target.as_object_mut() else {
        return;
    };

    for (key, patch_value) in patch_map {
        if patch_value.is_null() {
            target_map.remove(key);
        } else {
            merge_patch(target_map.entry(key).or_insert(Value::Null), patch_value);
        }
    }
}

/// Clamp the requested memory ceiling to the process limit before validation.
///
/// A client may lower `max_memory_bytes` at runtime, but it can never raise it past the ceiling
/// the operator set on the command line. Clamping first (rather than after `validate`) means
/// validation compares the requested target against the ceiling that will actually apply.
fn apply_limits(state: &mut WorkloadState, limits: Limits) {
    state.controller.max_memory_bytes = state
        .controller
        .max_memory_bytes
        .min(limits.max_memory_bytes);
}

/// Map every `Json` extractor failure to 400.
///
/// Axum's defaults split these across 400 (syntax), 415 (missing content-type) and 422 (schema
/// mismatch). The API contract is "malformed or invalid desired state is 400", and the error body
/// shape should match every other failure.
fn json_rejection(rejection: JsonRejection) -> LoadlabError {
    LoadlabError::InvalidConfig(rejection.body_text())
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::merge_patch;
    use serde_json::json;

    #[test]
    fn merge_patch_changes_only_requested_field() {
        let mut target = json!({
            "running": true,
            "controller": { "tick_ms": 100, "tolerance_bytes": 10 },
            "memory": { "anonymous_rss": "128MiB" }
        });
        merge_patch(&mut target, &json!({"memory": {"anonymous_rss": "256MiB"}}));
        assert_eq!(target["memory"]["anonymous_rss"], "256MiB");
        assert_eq!(target["controller"]["tick_ms"], 100);
        assert_eq!(target["running"], true);
    }

    #[test]
    fn merge_patch_null_removes_key() {
        let mut target = json!({ "memory": { "anonymous_rss": "128MiB" }, "running": true });
        merge_patch(&mut target, &json!({ "memory": { "anonymous_rss": null } }));
        assert_eq!(target, json!({ "memory": {}, "running": true }));
    }

    #[test]
    fn merge_patch_scalar_replaces_whole_object() {
        let mut target = json!({ "memory": { "anonymous_rss": "128MiB" } });
        merge_patch(&mut target, &json!({ "memory": "flat" }));
        assert_eq!(target, json!({ "memory": "flat" }));
    }

    #[test]
    fn merge_patch_creates_missing_intermediate_objects() {
        let mut target = json!({});
        merge_patch(
            &mut target,
            &json!({ "memory": { "anonymous_rss": "64MiB" } }),
        );
        assert_eq!(target, json!({ "memory": { "anonymous_rss": "64MiB" } }));
    }

    #[test]
    fn merge_patch_on_non_object_target_yields_object() {
        let mut target = json!(7);
        merge_patch(&mut target, &json!({ "running": true }));
        assert_eq!(target, json!({ "running": true }));
    }
}
