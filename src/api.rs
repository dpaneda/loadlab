use std::sync::{Arc, RwLock};

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::header,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
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
    /// measurement's meaning depends on it: two runs of the same scenario on different hosts are
    /// the same series and different events.
    pub host: HostInfo,
    /// Built once at startup and shared by every scrape. Metric names are validated for
    /// uniqueness here rather than on each request.
    pub metrics: Arc<metrics::Metrics>,
    pub scenarios: Arc<RwLock<HashMap<String, Scenario>>>,
    /// Name of the scenario currently driving the workload, if any.
    ///
    /// Kept apart from the desired state rather than inside it, because it is a fact about *how*
    /// the state was installed, not part of the state itself. Any direct `/workload` write clears
    /// it, so it cannot outlive the request that set it.
    pub active_scenario: Arc<RwLock<Option<String>>>,
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
        .route("/scenarios", get(list_scenarios))
        .route("/scenarios/load", post(load_scenario))
        .route("/scenarios/run", post(run_scenario))
        .route("/scenarios/stop", post(stop_scenario))
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
    value.normalize();
    apply_limits(&mut value, app.limits);
    value.validate()?;
    *write_lock(&app.workload) = value.clone();
    // The desired state is now whatever this request said, not whatever a scenario said.
    *write_lock(&app.active_scenario) = None;
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
    next.normalize();
    apply_limits(&mut next, app.limits);
    next.validate()?;
    *guard = next.clone();
    // Same as `PUT`: the workload is no longer the scenario's.
    *write_lock(&app.active_scenario) = None;
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
    // A stopped workload is not being driven by a scenario, whatever set it last.
    *write_lock(&app.active_scenario) = None;
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
        experiment: ExperimentMetrics {
            seed: workload.experiment.seed,
            start_time: workload.experiment.start_time.clone(),
            elapsed_seconds: runtime.experiment.elapsed_seconds(),
            start_offset_seconds: runtime.experiment.start_offset_seconds(),
            active_scenario: active_scenario(&app),
        },
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

/// The longest scenario name or version accepted.
///
/// Both are client-supplied strings that get stored and echoed, and a scenario store is the one
/// place a client can add unbounded keys. Capping them keeps a single request from filling memory
/// with identifiers, and the limit is generous for the identifiers this API actually needs.
const SCENARIO_ID_MAX_LEN: usize = 128;

/// A stored, versioned workload definition.
///
/// `workload` is stored already normalized and validated, so applying it cannot produce a state the
/// direct `/workload` path would have rejected. That is what makes a scenario *deterministic* in
/// the sense that matters: the same document always yields the same desired state, whichever
/// spelling of the composite aliases it was written in.
#[derive(Clone, Debug, Serialize)]
pub struct Scenario {
    pub name: String,
    pub version: String,
    /// The desired state, normalized. `running` is always `false` here: a scenario describes *what*
    /// to generate, and whether it is generating is what `/scenarios/run` decides.
    pub workload: WorkloadState,
}

/// Body of `POST /scenarios/load`.
#[derive(Debug, Deserialize)]
struct LoadScenario {
    name: String,
    version: String,
    workload: serde_json::Value,
}

/// Body of `POST /scenarios/run` and `POST /scenarios/stop`.
#[derive(Debug, Deserialize)]
struct ScenarioName {
    name: String,
}

#[derive(Debug, Serialize)]
struct ScenarioList {
    /// Name of the scenario currently driving the workload, if any.
    ///
    /// `null` after a direct `PUT`/`PATCH`/`/workload/stop`, because at that point the desired
    /// state is no longer the scenario's. Reporting a scenario as active while the workload has
    /// been changed underneath it is the specific lie this field must not tell.
    active: Option<String>,
    scenarios: Vec<ScenarioSummary>,
}

#[derive(Debug, Serialize)]
struct ScenarioSummary {
    name: String,
    version: String,
}

/// Reject an identifier that is empty, over-long, or carries control characters.
///
/// Control characters are refused because a scenario name is echoed in JSON and in operator-facing
/// logs; a name containing a newline would let a caller forge extra lines in whatever reads them.
fn validate_scenario_id(field: &str, value: &str) -> Result<(), LoadlabError> {
    if value.trim().is_empty() {
        return Err(LoadlabError::InvalidConfig(format!(
            "scenario {field} must not be empty"
        )));
    }
    if value.len() > SCENARIO_ID_MAX_LEN {
        return Err(LoadlabError::InvalidConfig(format!(
            "scenario {field} must be at most {SCENARIO_ID_MAX_LEN} bytes"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(LoadlabError::InvalidConfig(format!(
            "scenario {field} must not contain control characters"
        )));
    }
    Ok(())
}

/// Look up a stored scenario by name.
fn scenario_or_404<'a>(
    scenarios: &'a HashMap<String, Scenario>,
    name: &str,
) -> Result<&'a Scenario, LoadlabError> {
    scenarios
        .get(name)
        .ok_or_else(|| LoadlabError::InvalidConfig(format!("no scenario named {name:?} is loaded")))
}

/// The canonical serialization of a state, used to decide whether two documents are the same one.
///
/// The aliases are already folded in by the time this is called, so two scenarios that differ only
/// in which spelling they used compare equal, which is what a version is supposed to mean.
fn canonical_form(workload: &WorkloadState) -> String {
    serde_json::to_string(workload).unwrap_or_default()
}

async fn list_scenarios(State(app): State<AppState>) -> Json<ScenarioList> {
    let scenarios = read_lock(&app.scenarios);
    let mut summaries: Vec<ScenarioSummary> = scenarios
        .values()
        .map(|scenario| ScenarioSummary {
            name: scenario.name.clone(),
            version: scenario.version.clone(),
        })
        .collect();
    // Sorted so the listing is stable across calls. `HashMap` iteration order is not, and an
    // endpoint whose output reorders itself between two identical requests is impossible to
    // diff.
    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    Json(ScenarioList {
        active: read_lock(&app.active_scenario).clone(),
        scenarios: summaries,
    })
}

/// Store a named, versioned scenario without running it.
async fn load_scenario(
    State(app): State<AppState>,
    payload: Result<Json<LoadScenario>, JsonRejection>,
) -> Result<Json<Scenario>, LoadlabError> {
    let Json(payload) = payload.map_err(json_rejection)?;
    validate_scenario_id("name", &payload.name)?;
    validate_scenario_id("version", &payload.version)?;

    let mut workload: WorkloadState = serde_json::from_value(payload.workload)
        .map_err(|error| LoadlabError::InvalidConfig(error.to_string()))?;
    workload.normalize();
    apply_limits(&mut workload, app.limits);
    workload.validate()?;
    // A stored scenario never carries `running`. Storing the flag would mean `run` had to decide
    // whether to honour it, and a scenario that remembers being started is not a definition.
    workload.running = false;

    let scenario = Scenario {
        name: payload.name.clone(),
        version: payload.version.clone(),
        workload,
    };

    let mut scenarios = write_lock(&app.scenarios);
    if let Some(existing) = scenarios.get(&payload.name) {
        // A version identifies a document. Re-loading a name with different content under the same
        // version would make `version` meaningless and quietly break the reproducibility the
        // version exists to provide, so it is refused rather than overwritten.
        if existing.version == payload.version
            && canonical_form(&existing.workload) != canonical_form(&scenario.workload)
        {
            return Err(LoadlabError::InvalidConfig(format!(
                "scenario {:?} version {:?} is already loaded with different content; bump the \
                 version to change it",
                payload.name, payload.version
            )));
        }
    }
    scenarios.insert(payload.name.clone(), scenario.clone());
    Ok(Json(scenario))
}

/// Apply a stored scenario and start it.
async fn run_scenario(
    State(app): State<AppState>,
    payload: Result<Json<ScenarioName>, JsonRejection>,
) -> Result<Json<WorkloadState>, LoadlabError> {
    let Json(payload) = payload.map_err(json_rejection)?;
    let scenario = {
        let scenarios = read_lock(&app.scenarios);
        scenario_or_404(&scenarios, &payload.name)?.clone()
    };

    // Re-validated on the way out even though it was validated on the way in. The process ceiling
    // is fixed, so this cannot change the verdict today; it means the guarantee "a scenario can
    // never install a state `/workload` would reject" is enforced by the code that installs it
    // rather than by remembering that it was checked earlier.
    let mut workload = scenario.workload.clone();
    apply_limits(&mut workload, app.limits);
    workload.validate()?;
    workload.running = true;

    *write_lock(&app.workload) = workload.clone();
    *write_lock(&app.active_scenario) = Some(scenario.name);
    Ok(Json(workload))
}

/// Stop the active scenario.
///
/// Equivalent to `POST /workload/stop` and deliberately so: a scenario drives the same single
/// desired state as any other client write, so "stop the scenario" and "stop the workload" cannot
/// mean different things. What this endpoint adds is clearing the active marker, so `GET /scenarios`
/// stops claiming a scenario is driving a workload that is no longer running.
async fn stop_scenario(State(app): State<AppState>) -> Json<WorkloadState> {
    let mut workload = write_lock(&app.workload);
    workload.running = false;
    *write_lock(&app.active_scenario) = None;
    Json(workload.clone())
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
    /// Which run these numbers belong to.
    experiment: ExperimentMetrics,
    workload: WorkloadState,
    controller: ControllerMetrics,
}

/// The identity of the current run.
///
/// Separate from `controller`, which describes the loop, and from `workload`, which describes what
/// was asked for: this block is what an observation has to be filed under to mean anything later.
#[derive(Debug, Serialize)]
struct ExperimentMetrics {
    /// The configured seed, or `null` when none is.
    seed: Option<u64>,
    /// The configured `t = 0`, or `null` when `t` is the age of the run.
    start_time: Option<String>,
    /// Seconds since `t = 0`, which is what the workload's expressions were evaluated against.
    elapsed_seconds: f64,
    /// Seconds between `start_time` and the moment the run was anchored.
    ///
    /// Present only with a configured `start_time`, and the thing that makes `elapsed_seconds`
    /// readable: with an offset of `-600`, an `elapsed_seconds` of 300 is five minutes *before*
    /// the experiment rather than five minutes into it.
    start_offset_seconds: Option<f64>,
    /// The scenario driving the workload, with the version that identifies its content.
    ///
    /// `null` for a workload installed directly, which is a different kind of run rather than an
    /// older one.
    active_scenario: Option<ActiveScenario>,
}

/// A scenario's name and the version of its content.
#[derive(Debug, Serialize)]
struct ActiveScenario {
    name: String,
    version: String,
}

/// Resolve the active scenario, if the scenario store still holds it.
///
/// Checked against the store on every scrape rather than cached, because `PUT /workload` clears the
/// marker but leaves the store populated: a marker pointing at a scenario that has since been
/// replaced is worse than no marker.
fn active_scenario(app: &AppState) -> Option<ActiveScenario> {
    let name = read_lock(&app.active_scenario).clone()?;
    let scenarios = read_lock(&app.scenarios);
    let scenario = scenarios.get(&name)?;
    Some(ActiveScenario {
        name: scenario.name.clone(),
        version: scenario.version.clone(),
    })
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
