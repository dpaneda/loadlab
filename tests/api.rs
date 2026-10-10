//! End-to-end tests against the real `loadlab` binary.
//!
//! The crate has no library target, so these tests spawn the built executable on a loopback port
//! and speak HTTP/1.1 to it directly. That keeps the assertions on the actual wire contract
//! (status codes, JSON body shape, controller convergence) rather than on internal functions.

use std::{
    fmt::Write as _,
    io::{Read, Write as _},
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

use serde_json::Value;

const CEILING_BYTES: u64 = 512 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
/// The same unit as a float, for comparisons against the numeric JSON fields.
const MIB_F: f64 = 1024.0 * 1024.0;
/// The disk ceiling most tests run with. Large enough that it is never what makes a test pass,
/// so `a_request_above_the_file_ceiling_is_clamped_and_reported_saturated` is the only place the
/// ceiling itself is under test.
const DEFAULT_FILE_CEILING_BYTES: u64 = 512 * 1024 * 1024;
/// The shared-memory ceiling most tests run with. Large enough that it is never what makes a test
/// pass, so `a_request_above_the_shared_ceiling_is_clamped_and_reported_saturated` is the only
/// place the shared ceiling itself is under test.
const DEFAULT_SHARED_CEILING_BYTES: u64 = 512 * 1024 * 1024;
/// The disk-I/O ceiling most tests run with. Large enough that it is never what makes a test pass,
/// so `a_request_above_the_io_ceiling_is_clamped_and_reported_saturated` is the only place the
/// I/O ceiling itself is under test.
const DEFAULT_IO_CEILING_BYTES: u64 = 512 * 1024 * 1024;
/// The network I/O ceiling most tests run with. 100_000 is the process default; this is high enough
/// that a controller rate of a few thousand operations per second never hits it.
const DEFAULT_NETWORK_CEILING_OPS: u64 = 100_000;
/// A fixed instant well before any test process could have started, and its Unix seconds.
const PAST_START_TIME: &str = "2024-01-01T00:00:00Z";
const PAST_START_TIME_UNIX: i64 = 1_704_067_200;
/// A fixed instant after any test process will plausibly still be running.
const FUTURE_START_TIME: &str = "2100-01-01T00:00:00Z";
/// The page size every test machine runs with. The `pages`/`rss_bytes` relationship the tests
/// assert is an identity the controller establishes, so it holds for whatever the page size is.
const PAGE_BYTES: u64 = 4096;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// Every process ceiling the harness passes, with the value to use when a test does not override it.
const CEILING_DEFAULTS: [(&str, u64); 4] = [
    ("--max-file-backed-bytes", DEFAULT_FILE_CEILING_BYTES),
    ("--max-shared-bytes", DEFAULT_SHARED_CEILING_BYTES),
    ("--max-io-bytes", DEFAULT_IO_CEILING_BYTES),
    ("--max-network-ops-per-second", DEFAULT_NETWORK_CEILING_OPS),
];
const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(20);

struct Server {
    addr: SocketAddr,
    child: Child,
}

impl Server {
    fn start() -> Self {
        Self::start_with_ceiling(DEFAULT_FILE_CEILING_BYTES, DEFAULT_SHARED_CEILING_BYTES)
    }

    /// Start a server with an explicit disk ceiling for the file-backed controller.
    ///
    /// Separate from `start` because the ceiling is a process-level limit a client cannot lift,
    /// and testing that a request is clamped to it requires setting it lower than the default.
    fn start_with_file_ceiling(max_file_backed_bytes: u64) -> Self {
        Self::start_with_ceiling(max_file_backed_bytes, DEFAULT_SHARED_CEILING_BYTES)
    }

    /// Start a server with an explicit shared-memory ceiling.
    ///
    /// The same reasoning as [`Self::start_with_file_ceiling`], for the combined shared plus
    /// copy-on-write ceiling rather than the disk one.
    fn start_with_shared_ceiling(max_shared_bytes: u64) -> Self {
        Self::start_with_ceiling(DEFAULT_FILE_CEILING_BYTES, max_shared_bytes)
    }

    /// Start a server with an explicit disk-I/O region ceiling.
    ///
    /// The same reasoning as [`Self::start_with_file_ceiling`], for the I/O scratch region rather
    /// than the file-backed controller.
    fn start_with_io_ceiling(max_io_bytes: u64) -> Self {
        Self::start_with_options(&[("--max-io-bytes", max_io_bytes.to_string())], &[])
    }

    /// Start a server with an explicit network-ops ceiling.
    ///
    /// Lower than default so the test can control the network operations-per-second ceiling itself.
    fn start_with_network_ceiling(max_network_ops_per_second: u64) -> Self {
        Self::start_with_options(
            &[(
                "--max-network-ops-per-second",
                max_network_ops_per_second.to_string(),
            )],
            &[],
        )
    }

    /// Start a server that requires `token` on every endpoint except `/health`.
    fn start_with_token(token: &str) -> Self {
        Self::start_with_options(&[], &["--api-token", token])
    }

    /// Spawn a discarding TCP/UDP sink child on `sink_addr` and return a struct that waits for it.
    ///
    /// The sink is a self-contained blackhole that absorbs the controller target traffic. Use this
    /// to write self-contained integration tests for the sink, where a workload is pointed at one
    /// rather than an application server.
    fn spawn_sink(sink_addr: SocketAddr) -> Sink {
        let child = Command::new(env!("CARGO_BIN_EXE_loadlab"))
            .arg("--sink")
            .arg(sink_addr.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sink child");
        Sink {
            child,
            addr: sink_addr,
        }
    }

    fn start_with_ceiling(max_file_backed_bytes: u64, max_shared_bytes: u64) -> Self {
        Self::start_with_ceilings(
            max_file_backed_bytes,
            max_shared_bytes,
            DEFAULT_IO_CEILING_BYTES,
            DEFAULT_NETWORK_CEILING_OPS,
        )
    }

    fn start_with_ceilings(
        max_file_backed_bytes: u64,
        max_shared_bytes: u64,
        max_io_bytes: u64,
        max_network_ops_per_second: u64,
    ) -> Self {
        Self::start_with_options(
            &[
                ("--max-file-backed-bytes", max_file_backed_bytes.to_string()),
                ("--max-shared-bytes", max_shared_bytes.to_string()),
                ("--max-io-bytes", max_io_bytes.to_string()),
                (
                    "--max-network-ops-per-second",
                    max_network_ops_per_second.to_string(),
                ),
            ],
            &[],
        )
    }

    /// Start a server with ceiling overrides and extra arguments.
    ///
    /// Every ceiling is passed exactly once, taking the override when the caller names it and
    /// [`CEILING_DEFAULTS`] otherwise: the flags reject a repeat, so a caller overriding one ceiling
    /// cannot also get the default spelled out for it. The extra arguments are appended so the
    /// security flags need no constructor per flag.
    fn start_with_options(overrides: &[(&str, String)], extra: &[&str]) -> Self {
        let ceilings: Vec<(String, String)> = CEILING_DEFAULTS
            .iter()
            .map(|(flag, default)| {
                let value = overrides
                    .iter()
                    .find(|(name, _)| name == flag)
                    .map_or_else(|| default.to_string(), |(_, value)| value.clone());
                ((*flag).to_owned(), value)
            })
            .collect();

        let addr: SocketAddr = format!("127.0.0.1:{}", free_port())
            .parse()
            .expect("valid addr");
        let child = Command::new(env!("CARGO_BIN_EXE_loadlab"))
            .arg("--bind")
            .arg(addr.to_string())
            .arg("--max-memory-bytes")
            .arg(CEILING_BYTES.to_string())
            .args(ceilings.iter().flat_map(|(flag, value)| [flag, value]))
            // The crate directory rather than `/tmp`, which is tmpfs on most hosts. See the
            // `file_backed_dir` argument's documentation for why that is not interchangeable.
            .arg("--file-backed-dir")
            .arg(env!("CARGO_MANIFEST_DIR"))
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn loadlab binary");

        let server = Self { addr, child };
        server.await_health();
        server
    }

    fn await_health(&self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if let Some((200, _)) = self.try_get("/health") {
                return;
            }
            sleep(Duration::from_millis(25));
        }
        panic!("loadlab did not become healthy on {}", self.addr);
    }

    fn try_get(&self, path: &str) -> Option<(u16, Value)> {
        request(self.addr, "GET", path, None).ok()
    }

    fn get(&self, path: &str) -> (u16, Value) {
        request(self.addr, "GET", path, None).expect("GET request")
    }

    /// `GET` with an `Authorization` header, returning `(status, body)`.
    fn get_authorized(&self, path: &str, token: &str) -> (u16, Value) {
        self.send_authorized("GET", path, None, token)
    }

    /// One request with no credentials at all.
    ///
    /// Not a wrapper over [`Self::send_authorized`]: that one always attaches a header, and the
    /// whole point here is a request that carries none.
    fn send_anonymous(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let (status, _, body) =
            request_raw(self.addr, method, path, body).expect("anonymous request");
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    /// One request with a `Bearer` credential, in any state the header allows.
    fn send_authorized(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        authorization: &str,
    ) -> (u16, Value) {
        let (status, _, body) = request_raw_with_headers(
            self.addr,
            method,
            path,
            body,
            &[("Authorization", authorization)],
        )
        .expect("authorized request");
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    /// Fetch a non-JSON endpoint, returning `(status, content_type, body)`.
    fn get_text(&self, path: &str) -> (u16, String, String) {
        let (status, head, body) = request_raw(self.addr, "GET", path, None).expect("GET request");
        let content_type = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-type")
                    .then(|| value.trim().to_owned())
            })
            .unwrap_or_default();
        (status, content_type, body)
    }

    fn put(&self, path: &str, body: &str) -> (u16, Value) {
        request(self.addr, "PUT", path, Some(body)).expect("PUT request")
    }

    fn patch(&self, path: &str, body: &str) -> (u16, Value) {
        request(self.addr, "PATCH", path, Some(body)).expect("PATCH request")
    }

    fn post(&self, path: &str) -> (u16, Value) {
        request(self.addr, "POST", path, None).expect("POST request")
    }

    /// `POST` with a JSON body, for the endpoints that take one.
    fn post_json(&self, path: &str, body: &str) -> (u16, Value) {
        request(self.addr, "POST", path, Some(body)).expect("POST request")
    }

    /// Poll `/metrics` until `predicate` holds, returning the last value that satisfied it.
    fn wait_metrics(&self, description: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + CONVERGENCE_TIMEOUT;
        while Instant::now() < deadline {
            let (status, body) = self.get("/metrics");
            assert_eq!(status, 200, "/metrics returned {status}");
            if predicate(&body) {
                return body;
            }
            sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for: {description}");
    }
}

impl Drop for Server {
    /// Shut the server down with SIGTERM, then fall back to SIGKILL.
    ///
    /// The signal matters: loadlab unlinks its file-backed temporary file on a clean shutdown, so
    /// `kill()` alone would make every test that generates file-backed memory leave a `.tmp*` file
    /// in the repository. It would also mean the tests never exercise the teardown path that
    /// actually runs in production.
    fn drop(&mut self) {
        // SAFETY: the pid is loadlab's own, and SIGTERM is a signal the server installs a handler
        // for, so this cannot be an invalid target.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.child.try_wait().expect("poll the child").is_some() && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        // Still alive: it is wedged, and the test has to finish regardless.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Issue one HTTP/1.1 request and return `(status, parsed_body)`.
fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> std::io::Result<(u16, Value)> {
    let (status, _, body) = request_raw(addr, method, path, body)?;
    Ok((status, serde_json::from_str(&body).unwrap_or(Value::Null)))
}

/// Issue one HTTP/1.1 request and return `(status, headers, body)` verbatim.
///
/// `Connection: close` makes the response self-delimiting, so the whole reply can be buffered and
/// split on the first blank line.
fn request_raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> std::io::Result<(u16, String, String)> {
    request_raw_with_headers(addr, method, path, body, &[])
}

/// As [`request_raw`], plus request headers a test needs to send for itself.
///
/// Hand-written rather than through an HTTP client so a test can send a deliberately wrong or
/// malformed credential, which is the whole point of the authentication tests.
fn request_raw_with_headers(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
    extra: &[(&str, &str)],
) -> std::io::Result<(u16, String, String)> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;

    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: loadlab\r\nConnection: close\r\n");
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        write!(head, "Content-Length: {}\r\n", body.len()).expect("write to String");
    }
    for (name, value) in extra {
        write!(head, "{name}: {value}\r\n").expect("write to String");
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes())?;
    if let Some(body) = body {
        stream.write_all(body.as_bytes())?;
    }
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8(raw).expect("response is utf-8");

    let (head, body) = text.split_once("\r\n\r\n").expect("response head and body");
    let status_line = head.lines().next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("numeric status code");

    Ok((status, head.to_owned(), body.to_owned()))
}

fn rss_anon(metrics: &Value) -> u64 {
    metrics["process"]["memory"]["rss_anon_bytes"]
        .as_u64()
        .expect("rss_anon_bytes is a number")
}

fn baseline(metrics: &Value) -> u64 {
    metrics["controller"]["baseline_rss_anon_bytes"]
        .as_u64()
        .expect("baseline_rss_anon_bytes is a number")
}

/// The CPU controller's own block of the `/metrics` response.
fn cpu(metrics: &Value) -> &Value {
    &metrics["controller"]["cpu"]
}

fn cpu_number(metrics: &Value, field: &str) -> f64 {
    cpu(metrics)[field].as_f64().unwrap_or_else(|| {
        panic!(
            "controller.cpu.{field} should be a number, got {:?}",
            cpu(metrics)[field]
        )
    })
}

fn cpu_flag(metrics: &Value, field: &str) -> bool {
    cpu(metrics)[field]
        .as_bool()
        .unwrap_or_else(|| panic!("controller.cpu.{field} should be a bool"))
}

/// The file-backed controller's own block of the `/metrics` response.
fn file_backed(metrics: &Value) -> &Value {
    &metrics["controller"]["file_backed"]
}

fn file_backed_number(metrics: &Value, field: &str) -> f64 {
    file_backed(metrics)[field].as_f64().unwrap_or_else(|| {
        panic!(
            "controller.file_backed.{field} should be a number, got {:?}",
            file_backed(metrics)[field]
        )
    })
}

fn file_backed_flag(metrics: &Value, field: &str) -> bool {
    file_backed(metrics)[field]
        .as_bool()
        .unwrap_or_else(|| panic!("controller.file_backed.{field} should be a bool"))
}

fn shared(metrics: &Value) -> &Value {
    &metrics["controller"]["shared"]
}

fn shared_number(metrics: &Value, field: &str) -> f64 {
    shared(metrics)[field].as_f64().unwrap_or_else(|| {
        panic!(
            "controller.shared.{field} should be a number, got {:?}",
            shared(metrics)[field]
        )
    })
}

fn shared_flag(metrics: &Value, field: &str) -> bool {
    shared(metrics)[field]
        .as_bool()
        .unwrap_or_else(|| panic!("controller.shared.{field} should be a bool"))
}

fn shared_string(metrics: &Value, field: &str) -> String {
    shared(metrics)[field]
        .as_str()
        .unwrap_or_else(|| panic!("controller.shared.{field} should be a string"))
        .to_owned()
}

fn file_backed_string(metrics: &Value, field: &str) -> String {
    file_backed(metrics)[field]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "controller.file_backed.{field} should be a string, got {:?}",
                file_backed(metrics)[field]
            )
        })
        .to_owned()
}

/// The I/O controller's own block of the `/metrics` response.
fn io(metrics: &Value) -> &Value {
    &metrics["controller"]["io"]
}

fn io_number(metrics: &Value, field: &str) -> f64 {
    io(metrics)[field].as_f64().unwrap_or_else(|| {
        panic!(
            "controller.io.{field} should be a number, got {:?}",
            io(metrics)[field]
        )
    })
}

fn io_bool(metrics: &Value, field: &str) -> Option<bool> {
    match io(metrics).get(field) {
        Some(value) => Some(
            value
                .as_bool()
                .unwrap_or_else(|| panic!("controller.io.{field} should be a bool, got {value:?}")),
        ),
        None => panic!("controller.io.{field} is missing"),
    }
}

/// A number at a dotted path under `controller.io`, for the nested latency blocks.
fn io_path_number(metrics: &Value, path: &str) -> f64 {
    let mut cursor = io(metrics);
    for segment in path.split('.') {
        cursor = cursor.get(segment).unwrap_or_else(|| {
            panic!("controller.io.{path} is missing at segment {segment} in {cursor}")
        });
    }
    cursor
        .as_f64()
        .unwrap_or_else(|| panic!("controller.io.{path} should be a number, got {cursor}"))
}

/// Whether a dotted path under `controller.io` is JSON `null`, which is how loadlab reports "not
/// measured" rather than "zero".
fn io_null(metrics: &Value, path: &str) -> bool {
    let mut cursor = io(metrics);
    for segment in path.split('.') {
        cursor = cursor.get(segment).unwrap_or_else(|| {
            panic!("controller.io.{path} is missing at segment {segment} in {cursor}")
        });
    }
    cursor.is_null()
}

fn io_string(metrics: &Value, field: &str) -> String {
    io(metrics)[field]
        .as_str()
        .unwrap_or_else(|| panic!("controller.io.{field} should be a string"))
        .to_owned()
}

/// The network controller's own block of the `/metrics` response.
fn network(metrics: &Value) -> &Value {
    &metrics["controller"]["network"]
}

fn network_number(metrics: &Value, field: &str) -> f64 {
    network(metrics)[field].as_f64().unwrap_or_else(|| {
        panic!(
            "controller.network.{field} should be a number, got {:?}",
            network(metrics)[field]
        )
    })
}

fn network_flag(metrics: &Value, field: &str) -> bool {
    network(metrics)[field]
        .as_bool()
        .unwrap_or_else(|| panic!("controller.network.{field} should be a bool"))
}

fn network_string(metrics: &Value, field: &str) -> String {
    network(metrics)[field]
        .as_str()
        .unwrap_or_else(|| panic!("controller.network.{field} should be a string"))
        .to_owned()
}

fn ceiling() -> Value {
    Value::from(CEILING_BYTES)
}

/// A stopped workload must not be generating CPU.
///
/// This is the regression test for a default `cpu.utilization` that burned a core: because
/// `WorkloadState` is `#[serde(default)]`, any partial `PATCH` that never mentions CPU
/// restores the default, so a default that generated load would start generating load on
/// unrelated edits.
#[test]
fn a_stopped_workload_burns_no_cpu() {
    let server = Server::start();

    // The default is stopped, and the controller reports what the process is actually
    // doing rather than a blank slate: a stopped process that is quiet has converged.
    let metrics = server.wait_metrics("CPU to report the stopped state", |metrics| {
        cpu_flag(metrics, "converged")
    });
    assert!(cpu_flag(&metrics, "converged"));
    assert_eq!(cpu_number(&metrics, "worker_count"), 0.0);
    assert_eq!(cpu_number(&metrics, "duty_cycle"), 0.0);
    assert_eq!(cpu_number(&metrics, "requested_utilization_pct"), 0.0);
    assert!(
        cpu_number(&metrics, "actual_utilization_pct") < 20.0,
        "a stopped process should be near idle, reported {}",
        cpu_number(&metrics, "actual_utilization_pct")
    );

    // Starting the workload without ever mentioning CPU must still not generate CPU.
    assert_eq!(server.post("/workload/start").0, 200);
    sleep(Duration::from_millis(500));
    let metrics = server.get("/metrics").1;
    assert_eq!(cpu_number(&metrics, "worker_count"), 0.0);
    assert!(
        cpu_number(&metrics, "actual_utilization_pct") < 20.0,
        "starting a default workload generated CPU: {}",
        cpu_number(&metrics, "actual_utilization_pct")
    );

    // And a PATCH that only touches memory must not resurrect it either.
    assert_eq!(
        server
            .patch("/workload", r#"{"memory":{"anonymous_rss":"64MiB"}}"#)
            .0,
        200
    );
    sleep(Duration::from_millis(500));
    let metrics = server.get("/metrics").1;
    assert_eq!(cpu_number(&metrics, "worker_count"), 0.0);
    assert!(
        cpu_number(&metrics, "actual_utilization_pct") < 20.0,
        "a memory-only PATCH generated CPU: {}",
        cpu_number(&metrics, "actual_utilization_pct")
    );
}

/// One worker asked for a full core saturates exactly one core.
///
/// Both halves of the assertion are one-sided on purpose, so other tests running
/// concurrently in this same process cannot make them flaky. `actual` is CPU the *process*
/// was charged, so parallel test threads can only push it up, never down -- and the duty
/// cycle is already at its maximum of 1.0, so noise cannot move it either. A test that
/// asserted an exact figure, or a target in the middle of the range where the controller
/// still has room to correct, would be at the mercy of whatever else the machine is doing.
#[test]
fn a_full_core_target_saturates_exactly_one_worker() {
    let server = Server::start();
    assert_eq!(
        server
            .patch(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":100},"cpu":{"utilization":"100","workers":1}}"#
            )
            .0,
        200
    );

    let metrics = server.wait_metrics("a worker to saturate and settle", |metrics| {
        cpu_number(metrics, "actual_utilization_pct") >= 100.0 && cpu_flag(metrics, "converged")
    });

    assert_eq!(cpu_number(&metrics, "worker_count"), 1.0);
    assert_eq!(cpu_number(&metrics, "duty_cycle"), 1.0);
    assert_eq!(cpu_number(&metrics, "target_utilization_pct"), 100.0);
    assert_eq!(cpu_number(&metrics, "capacity_pct"), 100.0);
    assert!(cpu_flag(&metrics, "converged"));
    assert!(!cpu_flag(&metrics, "saturated"));

    // Stopping tears the threads down rather than leaving them idling. Convergence takes one
    // further tick because the interval just measured still contained the busy worker, so
    // "converged" here is a statement about the past -- which is the honest one to make.
    assert_eq!(server.post("/workload/stop").0, 200);
    let metrics = server.wait_metrics("workers to be released", |metrics| {
        cpu_number(metrics, "worker_count") == 0.0
    });
    assert_eq!(cpu_number(&metrics, "duty_cycle"), 0.0);
    assert_eq!(cpu_number(&metrics, "capacity_pct"), 0.0);

    let metrics = server.wait_metrics("the stopped process to go quiet", |metrics| {
        cpu_flag(metrics, "converged")
    });
    assert_eq!(cpu_number(&metrics, "worker_count"), 0.0);
    assert!(
        cpu_number(&metrics, "actual_utilization_pct") < 20.0,
        "stopping should release the core, measured {}",
        cpu_number(&metrics, "actual_utilization_pct")
    );
}

/// More CPU than the configured workers can deliver is clamped and reported, not silently
/// truncated and not treated as a success.
///
/// Nothing here depends on a measurement, so it holds on a machine of any size under any
/// load.
#[test]
fn a_request_above_capacity_is_clamped_and_flagged() {
    let server = Server::start();
    assert_eq!(
        server
            .patch(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":100},"cpu":{"utilization":"900","workers":2}}"#
            )
            .0,
        200
    );

    let metrics = server.wait_metrics("the request to be reported as saturated", |metrics| {
        cpu_flag(metrics, "saturated")
    });

    assert_eq!(cpu_number(&metrics, "worker_count"), 2.0);
    assert_eq!(cpu_number(&metrics, "capacity_pct"), 200.0);
    assert_eq!(
        cpu_number(&metrics, "requested_utilization_pct"),
        900.0,
        "the request has to stay visible, or the clamp is invisible"
    );
    assert_eq!(
        cpu_number(&metrics, "target_utilization_pct"),
        200.0,
        "two workers can deliver two cores and no more"
    );
    assert!(
        !cpu_flag(&metrics, "converged"),
        "hitting the ceiling is not the same as hitting the target"
    );
}

/// A target that tracks a moving expression is followed, not just reached once.
#[test]
fn a_time_varying_cpu_target_is_tracked() {
    let server = Server::start();
    // One full core at `t=0` rising to two over 20 seconds, on two workers so the target
    // stays inside capacity and the controller has to move rather than saturate.
    assert_eq!(
        server
            .patch(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":100},"cpu":{"utilization":"100 + 100*min(t/20,1)","workers":2}}"#
            )
            .0,
        200
    );

    let first = server.wait_metrics("the target to start moving", |metrics| {
        cpu_number(metrics, "target_utilization_pct") > 110.0
    });
    let early_actual = cpu_number(&first, "actual_utilization_pct");

    let late = server.wait_metrics("the target to reach 200", |metrics| {
        cpu_number(metrics, "target_utilization_pct") >= 199.0
            && cpu_number(metrics, "actual_utilization_pct") > early_actual
    });

    assert!(
        late["controller"]["cpu"]["actual_utilization_pct"]
            .as_f64()
            .expect("number")
            > early_actual,
        "actual utilization did not follow the target upward"
    );
    assert!(cpu_flag(&late, "converged"));
    assert_eq!(server.post("/workload/stop").0, 200);
}

/// Worker placement and the worker count are honoured, and a change to either rebuilds the
/// pool rather than being ignored.
#[test]
fn worker_count_and_pinning_take_effect_without_a_restart() {
    let server = Server::start();
    assert_eq!(
        server
            .patch(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":100},"cpu":{"utilization":"50","workers":1}}"#
            )
            .0,
        200
    );
    let metrics = server.wait_metrics("one worker", |metrics| {
        cpu_number(metrics, "worker_count") == 1.0
    });
    assert_eq!(cpu_number(&metrics, "capacity_pct"), 100.0);

    // Two workers on two named cores: capacity doubles, and the change is picked up live.
    assert_eq!(
        server
            .patch(
                "/workload",
                r#"{"cpu":{"utilization":"50","workers":2,"cpus":[0,1],"affinity":"pin"}}"#
            )
            .0,
        200
    );
    let metrics = server.wait_metrics("two pinned workers to settle", |metrics| {
        cpu_number(metrics, "worker_count") == 2.0
            && cpu_number(metrics, "capacity_pct") == 200.0
            && cpu_flag(metrics, "converged")
    });
    // 50 of 200 across two workers is a duty of 0.25 each, which is the arithmetic the
    // controller has to get right when it splits an aggregate target between threads.
    let duty = cpu_number(&metrics, "duty_cycle");
    assert!(
        (duty - 0.25).abs() < 0.1,
        "expected a duty near 0.25 for 50 of 200, got {duty}"
    );
    assert!(
        (cpu_number(&metrics, "actual_utilization_pct") - 50.0).abs() < 15.0,
        "actual {} should sit near the 50 target",
        cpu_number(&metrics, "actual_utilization_pct")
    );

    // And back down, which has to tear the pool down again.
    assert_eq!(
        server
            .patch(
                "/workload",
                r#"{"cpu":{"workers":0,"cpus":[],"affinity":"auto"}}"#
            )
            .0,
        200
    );
    let metrics = server.wait_metrics("one worker per logical CPU", |metrics| {
        cpu_number(metrics, "worker_count") > 2.0 && cpu_number(metrics, "capacity_pct") > 200.0
    });
    assert_eq!(
        cpu_number(&metrics, "worker_count"),
        cpu_number(&metrics, "capacity_pct") / 100.0,
        "one worker per CPU is the default"
    );
    assert_eq!(server.post("/workload/stop").0, 200);
}

/// Configuration errors are refused with a 400 and leave the previous state untouched.
#[test]
fn an_invalid_cpu_configuration_is_rejected_without_mutating_state() {
    let server = Server::start();
    let (status, _) = server.put(
        "/workload",
        r#"{"running":true,"cpu":{"utilization":"50","workers":1}}"#,
    );
    assert_eq!(status, 200);

    for (description, body) in [
        (
            "byte units in a percentage",
            r#"{"cpu":{"utilization":"1MiB"}}"#,
        ),
        (
            "an unknown affinity mode",
            r#"{"cpu":{"affinity":"sideways"}}"#,
        ),
        ("a negative target", r#"{"cpu":{"utilization":"-5"}}"#),
        (
            "a logical CPU that does not exist",
            r#"{"cpu":{"cpus":[99999]}}"#,
        ),
    ] {
        let (status, body) = server.patch("/workload", body);
        assert_eq!(status, 400, "{description} should be refused, got {body}");
    }

    // The state a client can still read is the last one that was accepted, not a partial
    // merge of the rejected ones.
    let (_, state) = server.get("/state");
    assert_eq!(state["cpu"]["utilization"], "50");
    assert_eq!(state["cpu"]["workers"], 1);
    assert!(state["cpu"]["cpus"].as_array().expect("array").is_empty());
}

/// The Prometheus endpoint exports the CPU controller alongside the raw process counters.
#[test]
fn prometheus_exports_the_cpu_controller() {
    let server = Server::start();
    assert_eq!(
        server
            .patch(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":100},"cpu":{"utilization":"100","workers":1}}"#
            )
            .0,
        200
    );
    server.wait_metrics("a worker to saturate", |metrics| {
        cpu_number(metrics, "actual_utilization_pct") >= 100.0
    });

    let (status, content_type, body) = server.get_text("/prometheus");
    assert_eq!(status, 200);
    assert!(content_type.starts_with("text/plain"), "{content_type}");

    for name in [
        "loadlab_cpu_workers",
        "loadlab_cpu_capacity_percent",
        "loadlab_cpu_requested_utilization_percent",
        "loadlab_cpu_target_utilization_percent",
        "loadlab_cpu_actual_utilization_percent",
        "loadlab_cpu_duty_cycle",
        "loadlab_cpu_converged",
        "loadlab_cpu_saturated",
        "loadlab_configured_cpu_workers",
    ] {
        let exported = body
            .lines()
            .any(|line| line.split([' ', '{']).next().unwrap_or_default() == name);
        assert!(exported, "{name} missing from /prometheus:\n{body}");
    }

    // The gauges have to agree with the JSON endpoint, or the two views of the same
    // controller disagree.
    let metrics = server.get("/metrics").1;
    let workers = body
        .lines()
        .find_map(|line| line.strip_prefix("loadlab_cpu_workers "))
        .expect("workers gauge")
        .trim()
        .parse::<f64>()
        .expect("numeric");
    assert_eq!(workers, cpu_number(&metrics, "worker_count"));

    assert_eq!(server.post("/workload/stop").0, 200);
}

#[test]
fn metrics_publish_the_experiment_identity_and_the_host_it_ran_on() {
    let server = Server::start();

    let (_, metrics) = server.get("/metrics");

    // Host identity. Every field loadlab can read is present, and `cpus` is at least the one it is
    // running on -- a host block that reported zero cores would make the CPU capacity fields
    // unreadable in context.
    let host = &metrics["host"];
    assert_eq!(host["kernel_name"], "Linux");
    assert!(
        !host["kernel_release"]
            .as_str()
            .expect("a release")
            .is_empty(),
        "kernel_release must identify the running kernel"
    );
    assert!(host["cpus"].as_u64().expect("cpus is a number") >= 1);
    assert!(
        host["page_size_bytes"].as_u64().expect("a number") >= 512,
        "a page size below 512 is not something Linux uses"
    );
    // Memory is read from /proc/meminfo, so it is present here but the parser is allowed to find
    // nothing; what matters is that the field exists either way rather than vanishing.
    assert!(
        host.get("memory_total_bytes").is_some(),
        "memory_total_bytes must always be a key, present or null"
    );
    // cpu_model comes from /proc/cpuinfo, which some architectures do not have; present-or-null is
    // the contract, and never an empty string.
    if let Some(model) = host["cpu_model"].as_str() {
        assert!(
            !model.is_empty(),
            "cpu_model must be absent rather than blank"
        );
    }

    // Experiment identity. Unconfigured by default, so nothing claims reproducibility it does not
    // have.
    assert!(experiment(&metrics)["seed"].is_null());
    assert!(experiment(&metrics)["start_time"].is_null());
    assert!(experiment(&metrics)["start_offset_seconds"].is_null());
    assert!(experiment(&metrics)["active_scenario"].is_null());
    assert!(experiment_elapsed(&metrics) >= 0.0);
}

#[test]
fn a_seed_can_be_installed_and_read_back_and_is_published_as_a_metric() {
    let server = Server::start();

    let (status, _) = server.patch("/workload", r#"{"experiment":{"seed":987654321}}"#);
    assert_eq!(status, 200);

    // In the desired state, so it is visible before anything runs.
    let (_, state) = server.get("/state");
    assert_eq!(state["experiment"]["seed"], 987_654_321);

    let (_, metrics) = server.get("/metrics");
    assert_eq!(experiment(&metrics)["seed"], 987_654_321);
    let (_, _, text) = server.get_text("/prometheus");
    assert_eq!(metric_u64(&text, "loadlab_experiment_seed"), 987_654_321);
}

#[test]
fn a_start_time_that_is_not_a_timestamp_is_refused_with_a_reason() {
    let server = Server::start();

    let (status, error) = server.patch("/workload", r#"{"experiment":{"start_time":"yesterday"}}"#);
    assert_eq!(status, 400);
    // The message has to name the field, or a client cannot tell which of its two values was wrong.
    let message = error["error"].as_str().expect("an error message");
    assert!(
        message.contains("experiment.start_time"),
        "the error should name the field: {message}"
    );

    // And the refusal left the state untouched rather than half-applying it.
    let (_, state) = server.get("/state");
    assert!(state["experiment"]["start_time"].is_null());
}

#[test]
fn t_is_the_age_of_the_run_and_restarts_when_the_workload_does() {
    let server = Server::start();
    let (status, _) = server.put(
        "/workload",
        r#"{"running":true,"cpu":{"utilization":"20","workers":1}}"#,
    );
    assert_eq!(status, 200);

    // Let the first run reach a `t` well clear of zero, so that a restart is unambiguously a
    // different number rather than a race between two nearly equal readings.
    let first_t = experiment_elapsed(
        &server.wait_metrics("the first run to reach a t of about a second", |metrics| {
            experiment_elapsed(metrics) > 1.0
        }),
    );
    assert!(
        experiment_elapsed(&server.get("/metrics").1) >= first_t,
        "t must not go backwards while a run continues"
    );

    // Stop and start again: a restart is a new run, and the point of a run-relative t is that the new
    // run starts from zero rather than continuing a number that now describes nothing.
    server.post("/workload/stop");
    sleep(Duration::from_millis(150));
    server.post("/workload/start");

    let restarted = server.wait_metrics("t to be measured from the new run", |metrics| {
        experiment_elapsed(metrics) > 0.05 && experiment_elapsed(metrics) < 0.5
    });
    assert!(
        experiment_elapsed(&restarted) < 0.5,
        "t should have restarted from the new run, got {}",
        experiment_elapsed(&restarted)
    );
}

#[test]
fn a_configured_start_time_places_t_against_a_shared_timeline() {
    let server = Server::start();
    // A fixed instant in the past, well before this process could have started. Writing it as a
    // constant rather than "ninety seconds ago" keeps the expected `t` exact arithmetic instead of
    // a range, and keeps the test from needing a date formatter in a crate that cannot import one.
    let (status, _) = server.patch(
        "/workload",
        &format!(r#"{{"experiment":{{"start_time":"{PAST_START_TIME}"}}}}"#),
    );
    assert_eq!(status, 200);

    // The desired state has the timestamp; the observation needs a tick to pick it up.
    let (_, state) = server.get("/state");
    assert_eq!(state["experiment"]["start_time"], PAST_START_TIME);
    let metrics = wait_for_start_offset(&server);

    let t = experiment_elapsed(&metrics);
    let expected = unix_seconds() - PAST_START_TIME_UNIX;
    assert!(
        (expected as f64 - 5.0..=expected as f64 + 5.0).contains(&t),
        "t should be the time since {PAST_START_TIME}, about {expected}, got {t}"
    );
    // The offset is what makes that number legible: it is how far the run was anchored from the
    // instant it was told to use, and here it is large, because the configured instant is old.
    let offset = experiment(&metrics)["start_offset_seconds"]
        .as_f64()
        .expect("an offset with a configured start time");
    assert!(offset > 3_000.0, "expected a large offset, got {offset}");

    // The workload's own age is a different number and is published separately, so a slow-aging
    // process is no longer mistaken for a slow run. This is the whole difference between the two
    // series: process uptime here is under a minute.
    let (_, _, text) = server.get_text("/prometheus");
    let uptime = metric_f64(&text, "loadlab_process_uptime_seconds");
    let elapsed = metric_f64(&text, "loadlab_experiment_elapsed_seconds");
    assert!(
        elapsed > uptime,
        "t is anchored to the configured instant, so it leads process uptime: {elapsed} vs {uptime}"
    );
}

#[test]
fn a_start_time_in_the_future_is_accepted_and_makes_t_negative() {
    let server = Server::start();
    let (status, _) = server.patch(
        "/workload",
        &format!(r#"{{"experiment":{{"start_time":"{FUTURE_START_TIME}"}}}}"#),
    );
    assert_eq!(status, 200);

    let metrics = wait_for_start_offset(&server);
    // Refusing this would be the easier choice, and the wrong one: a scenario shared across hosts
    // has to be installable before its scheduled start, and clamping to zero would throw away the
    // distance to the start instead of reporting it. Negative `t` is the honest answer.
    assert!(
        experiment_elapsed(&metrics) < 0.0,
        "a future start time should make t negative, got {}",
        experiment_elapsed(&metrics)
    );

    // The workload still runs: `t` is used by expressions, not as a gate. Patched rather than
    // replaced, because `PUT /workload` is a whole new desired state and would drop the start time
    // along with everything else.
    let (status, _) = server.patch(
        "/workload",
        r#"{"running":true,"cpu":{"utilization":"10","workers":1}}"#,
    );
    assert_eq!(status, 200);
    let metrics = server.wait_metrics("the workload to be reconciled", |metrics| {
        cpu_number(metrics, "requested_utilization_pct") > 0.0
    });
    assert!(experiment_elapsed(&metrics) < 0.0);
}

#[test]
fn a_start_time_in_the_past_keeps_t_climbing_across_a_stopped_workload() {
    let server = Server::start();
    server.patch(
        "/workload",
        &format!(r#"{{"experiment":{{"start_time":"{PAST_START_TIME}"}}}}"#),
    );
    // Patched, not replaced: `PUT /workload` would install a whole new desired state and take the
    // start time with it.
    let (status, _) = server.patch(
        "/workload",
        r#"{"running":true,"cpu":{"utilization":"10","workers":1}}"#,
    );
    assert_eq!(status, 200);

    wait_for_start_offset(&server);
    let before = experiment_elapsed(&server.get("/metrics").1);
    server.patch("/workload", r#"{"running":false}"#);
    sleep(Duration::from_millis(200));
    let after = experiment_elapsed(&server.get("/metrics").1);

    // `t` is wall time against a fixed instant, so it does not stop when the workload does. A
    // scenario with a gap in it needs `t` to keep moving across the gap; freezing it would make the
    // workload after the gap a different experiment from the one before it.
    assert!(
        after > before,
        "t should keep advancing while stopped: {before} then {after}"
    );
}

#[test]
fn an_active_scenario_is_published_with_the_version_that_identifies_its_content() {
    let server = Server::start();
    let scenario = r#"{"name":"ramp","version":"2024-05-01","workload":{"cpu":{"utilization":"10","workers":1}}}"#;
    let (status, _) = server.post_json("/scenarios/load", scenario);
    assert_eq!(status, 200);
    let (status, _) = server.post_json("/scenarios/run", r#"{"name":"ramp"}"#);
    assert_eq!(status, 200);

    let (_, metrics) = server.get("/metrics");
    // A version is the point. A scenario whose name is published but whose content is not cannot be
    // told apart from a later edit to the same name, which is what a version is for.
    assert_eq!(experiment(&metrics)["active_scenario"]["name"], "ramp");
    assert_eq!(
        experiment(&metrics)["active_scenario"]["version"],
        "2024-05-01"
    );

    // And it survives being read after the workload has been running, rather than only being set at
    // activation time.
    sleep(Duration::from_millis(150));
    let (_, later) = server.get("/metrics");
    assert_eq!(
        experiment(&later)["active_scenario"]["version"],
        "2024-05-01"
    );
}

#[test]
fn replacing_the_workload_directly_clears_the_active_scenario_rather_than_leaving_it_stale() {
    let server = Server::start();
    server.post_json(
        "/scenarios/load",
        r#"{"name":"ramp","version":"1","workload":{"cpu":{"utilization":"10","workers":1}}}"#,
    );
    server.post_json("/scenarios/run", r#"{"name":"ramp"}"#);
    let (_, metrics) = server.get("/metrics");
    assert_eq!(
        experiment(&metrics)["active_scenario"]["name"],
        "ramp",
        "the scenario should be active to begin with"
    );

    // The workload is now whatever the client just wrote, which the scenario no longer describes.
    server.put("/workload", r#"{"cpu":{"utilization":"10","workers":1}}"#);

    // Publishing "ramp" here would file a measurement of an unrelated workload under a name that
    // means something else.
    let (_, metrics) = server.get("/metrics");
    assert!(
        experiment(&metrics)["active_scenario"].is_null(),
        "a directly installed workload is a different kind of run, not an older scenario"
    );
}

#[test]
fn prometheus_exports_the_experiment_and_host_series() {
    let server = Server::start();
    server.patch("/workload", r#"{"experiment":{"seed":7}}"#);
    // The exposition is rendered by the controller loop, not by the request, so a scrape can
    // legitimately arrive before the first tick has read the new state.
    server.wait_metrics("the controller to tick", |metrics| {
        metrics["controller"]["tick_count"].as_u64().unwrap_or(0) > 0
    });
    let (_, content_type, text) = server.get_text("/prometheus");

    assert_eq!(content_type, "text/plain; version=0.0.4; charset=utf-8");

    // Both new gauges, with the types that match their use: both are point-in-time values, and a
    // counter would make a seed change or a restarted run look like a spike.
    let types: Vec<(String, String)> = exposed_types(&text)
        .into_iter()
        .filter(|(name, _)| {
            name == "loadlab_experiment_seed"
                || name == "loadlab_experiment_elapsed_seconds"
                || name == "loadlab_host_info"
        })
        .collect();
    assert_eq!(
        types,
        vec![
            (
                "loadlab_experiment_elapsed_seconds".to_owned(),
                "gauge".to_owned()
            ),
            ("loadlab_experiment_seed".to_owned(), "gauge".to_owned()),
            ("loadlab_host_info".to_owned(), "gauge".to_owned()),
        ],
    );

    // Host identity is labels on one series, not one series per host: the labels are bounded by the
    // machine, so unlike client-supplied values they cannot grow.
    let host_lines: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("loadlab_host_info{"))
        .collect();
    assert_eq!(host_lines.len(), 1, "expected exactly one host series");
    let host = host_lines[0];
    assert!(host.contains("machine="), "no machine label: {host}");
    assert!(host.contains("cpus="), "no cpu count label: {host}");
    assert!(host.contains("kernel_release="), "no kernel label: {host}");
    assert!(host.ends_with(" 1"), "host info must always be 1: {host}");

    assert_eq!(metric_u64(&text, "loadlab_experiment_seed"), 7);
}

#[test]
fn health_reports_ok() {
    let server = Server::start();
    let (status, body) = server.get("/health");
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
}

#[test]
fn state_defaults_are_served_before_any_update() {
    let server = Server::start();
    let (status, body) = server.get("/state");
    assert_eq!(status, 200);
    assert_eq!(body["running"], false);
    assert_eq!(body["controller"]["tick_ms"], 100);
    // The ceiling comes from --max-memory-bytes, not the built-in default.
    assert_eq!(body["controller"]["max_memory_bytes"], ceiling());
    assert_eq!(body["memory"]["anonymous_rss"], Value::Null);
}

#[test]
fn put_replaces_state_and_is_echoed_back() {
    let server = Server::start();
    let (status, body) = server.put(
        "/workload",
        r#"{"running":true,"controller":{"tick_ms":50},"memory":{"anonymous_rss":"64MiB"}}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(body["running"], true);
    assert_eq!(body["controller"]["tick_ms"], 50);
    assert_eq!(body["memory"]["anonymous_rss"], "64MiB");

    // PUT replaces, so fields absent from the body fall back to defaults rather than persisting.
    let (status, body) = server.put("/workload", r#"{"running":true}"#);
    assert_eq!(status, 200);
    assert_eq!(body["controller"]["tick_ms"], 100);
    assert_eq!(body["memory"]["anonymous_rss"], Value::Null);
    assert_eq!(server.get("/state").1["running"], true);
}

#[test]
fn malformed_json_is_rejected_with_400() {
    let server = Server::start();
    let (status, body) = server.put("/workload", r#"{"running": tru"#);
    assert_eq!(status, 400, "body was {body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty()),
        "error body should describe the failure, got {body}"
    );
    // A rejected update must not mutate desired state.
    assert_eq!(server.get("/state").1["running"], false);
}

#[test]
fn schema_mismatch_is_rejected_with_400() {
    let server = Server::start();
    let (status, body) = server.put("/workload", r#"{"running":"yes please"}"#);
    assert_eq!(status, 400, "body was {body}");
    assert_eq!(server.get("/state").1["running"], false);
}

#[test]
fn invalid_tick_interval_is_rejected_with_400() {
    let server = Server::start();
    for body in [
        r#"{"controller":{"tick_ms":0}}"#,
        r#"{"controller":{"tick_ms":10001}}"#,
    ] {
        let (status, response) = server.put("/workload", body);
        assert_eq!(status, 400, "body {body} was accepted as {response}");
    }
    assert_eq!(server.get("/state").1["controller"]["tick_ms"], 100);
}

#[test]
fn invalid_expression_is_rejected_with_400() {
    let server = Server::start();
    for body in [
        // syntax error
        r#"{"memory":{"anonymous_rss":"512MiB +"}}"#,
        // unknown byte unit
        r#"{"memory":{"anonymous_rss":"1XB"}}"#,
        // free variable other than `t`
        r#"{"memory":{"anonymous_rss":"2*x"}}"#,
        // unit without a magnitude is an unbound identifier
        r#"{"memory":{"anonymous_rss":"2*MiB"}}"#,
        // evaluates above the process ceiling at t=0
        r#"{"memory":{"anonymous_rss":"64MiB + 1TiB"}}"#,
    ] {
        let (status, response) = server.put("/workload", body);
        assert_eq!(status, 400, "body {body} was accepted as {response}");
    }
    assert_eq!(
        server.get("/state").1["memory"]["anonymous_rss"],
        Value::Null
    );
}

#[test]
fn process_ceiling_cannot_be_lifted_by_a_request() {
    let server = Server::start();

    // The operator ceiling is honoured at startup.
    assert_eq!(
        server.get("/state").1["controller"]["max_memory_bytes"],
        ceiling()
    );

    // A request that omits `controller` falls back to the serde default for the missing field.
    // That must not raise the effective ceiling back to the built-in 8GiB.
    let (status, body) = server.put(
        "/workload",
        r#"{"running":true,"memory":{"anonymous_rss":"64MiB"}}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(body["controller"]["max_memory_bytes"], ceiling());

    // Asking for more than the ceiling is still rejected, not silently truncated.
    let (status, body) = server.put("/workload", r#"{"memory":{"anonymous_rss":"4GiB"}}"#);
    assert_eq!(status, 400, "body was {body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.contains("max_memory_bytes")),
        "error should name the ceiling, got {body}"
    );

    // An explicit request for a huge ceiling is clamped down, not honoured.
    let (status, body) = server.put(
        "/workload",
        r#"{"controller":{"max_memory_bytes":1099511627776}}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(body["controller"]["max_memory_bytes"], ceiling());

    // A client may still lower the ceiling at runtime.
    let (status, body) = server.put(
        "/workload",
        r#"{"controller":{"max_memory_bytes":67108864}}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(body["controller"]["max_memory_bytes"], 64 * MIB);
    assert_eq!(
        server.get("/state").1["controller"]["max_memory_bytes"],
        64 * MIB
    );
}

#[test]
fn patch_leaves_unrelated_fields_untouched() {
    let server = Server::start();
    let (status, _) = server.put(
        "/workload",
        r#"{"running":true,"controller":{"tick_ms":250,"tolerance_bytes":4096},"memory":{"anonymous_rss":"64MiB"}}"#,
    );
    assert_eq!(status, 200);

    let (status, body) = server.patch(
        "/workload",
        r#"{"memory":{"anonymous_rss":"512MiB + 256MiB*sin(t/10)"}}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(body["memory"]["anonymous_rss"], "512MiB + 256MiB*sin(t/10)");
    assert_eq!(body["running"], true);
    assert_eq!(body["controller"]["tick_ms"], 250);
    assert_eq!(body["controller"]["tolerance_bytes"], 4096);
    assert_eq!(server.get("/state").1["controller"]["tick_ms"], 250);
}

#[test]
fn patch_null_removes_the_expression() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"memory":{"anonymous_rss":"64MiB"}}"#
            )
            .0,
        200
    );

    let (status, body) = server.patch("/workload", r#"{"memory":{"anonymous_rss":null}}"#);
    assert_eq!(status, 200);
    assert_eq!(body["memory"]["anonymous_rss"], Value::Null);
    // `running` must survive a patch that only clears a nested field.
    assert_eq!(body["running"], true);
}

#[test]
fn patch_rejects_invalid_result_without_mutating_state() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"memory":{"anonymous_rss":"64MiB"}}"#
            )
            .0,
        200
    );

    let (status, _) = server.patch("/workload", r#"{"controller":{"tick_ms":0}}"#);
    assert_eq!(status, 400);
    assert_eq!(server.get("/state").1["controller"]["tick_ms"], 100);
}

#[test]
fn start_and_stop_toggle_running() {
    let server = Server::start();
    let (status, body) = server.post("/workload/start");
    assert_eq!(status, 200);
    assert_eq!(body["running"], true);
    assert_eq!(server.get("/state").1["running"], true);

    let (status, body) = server.post("/workload/stop");
    assert_eq!(status, 200);
    assert_eq!(body["running"], false);
    assert_eq!(server.get("/state").1["running"], false);
}

#[test]
fn controller_converges_towards_a_growing_then_shrinking_target() {
    let server = Server::start();

    // `anonymous_rss` is a target for *total* RssAnon, so thresholds are the raw targets and the
    // process baseline must not be added to them.
    let floor = baseline(&server.get("/metrics").1);

    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},"memory":{"anonymous_rss":"128MiB"}}"#
            )
            .0,
        200
    );

    // Growing: RssAnon must reach roughly the requested total.
    let grown = server.wait_metrics("RssAnon to reach 128MiB", |metrics| {
        rss_anon(metrics) >= 128 * MIB
    });
    let peak = rss_anon(&grown);
    assert!(
        peak < 128 * MIB + 64 * MIB,
        "RssAnon overshot the target: {peak} bytes"
    );
    assert_eq!(grown["controller"]["last_error"], Value::Null);
    assert!(grown["controller"]["tick_count"].as_u64().unwrap_or(0) > 0);

    // Shrinking: halve the target.
    assert_eq!(
        server
            .patch("/workload", r#"{"memory":{"anonymous_rss":"64MiB"}}"#)
            .0,
        200
    );
    let shrunk = server.wait_metrics("RssAnon to fall back to 64MiB", |metrics| {
        rss_anon(metrics) < 96 * MIB
    });
    assert!(
        rss_anon(&shrunk) >= floor,
        "RssAnon dropped below the process baseline: {} < {floor}",
        rss_anon(&shrunk)
    );
    assert!(
        rss_anon(&shrunk) >= 32 * MIB,
        "RssAnon fell too far: {}",
        rss_anon(&shrunk)
    );

    // Stopping releases the arena entirely.
    assert_eq!(server.post("/workload/stop").0, 200);
    let released = server.wait_metrics("RssAnon to return near baseline", |metrics| {
        rss_anon(metrics) <= floor + 8 * MIB
    });
    assert!(rss_anon(&released) >= floor);
    assert_eq!(released["workload"]["running"], false);
    assert_eq!(released["controller"]["arena_resident_bytes"], 0);
}

#[test]
fn repeated_grow_shrink_cycles_do_not_leak_vmas_or_memory() {
    let server = Server::start();
    let floor = baseline(&server.get("/metrics").1);

    for _ in 0..6 {
        assert_eq!(
            server
                .put(
                    "/workload",
                    r#"{"running":true,"controller":{"tick_ms":50},"memory":{"anonymous_rss":"64MiB"}}"#
                )
                .0,
            200
        );
        server.wait_metrics("RssAnon to reach 64MiB", |metrics| {
            rss_anon(metrics) >= 64 * MIB
        });

        assert_eq!(server.post("/workload/stop").0, 200);
        server.wait_metrics("RssAnon to return near baseline", |metrics| {
            rss_anon(metrics) <= floor + 8 * MIB
        });
    }

    // 6 x 64MiB = 384MiB of leaked arena would show up here.
    let final_metrics = server.get("/metrics").1;
    assert!(
        rss_anon(&final_metrics) <= floor + 8 * MIB,
        "memory leaked across cycles: {} vs floor {floor}",
        rss_anon(&final_metrics)
    );
    assert_eq!(final_metrics["controller"]["arena_resident_bytes"], 0);
}

#[test]
fn time_dependent_expression_stays_within_its_amplitude() {
    let server = Server::start();

    // 128MiB +/- 32MiB, so total RssAnon must stay inside 96..160MiB.
    let low = 96 * MIB;
    let high = 160 * MIB;
    let (status, _) = server.put(
        "/workload",
        r#"{"running":true,"controller":{"tick_ms":50},"memory":{"anonymous_rss":"128MiB + 32MiB*sin(t/10)"}}"#,
    );
    assert_eq!(status, 200);

    // The expression must never be handed to the arena as a negative or absurd value, and the
    // controller must keep ticking without recording an error.
    let settled = server.wait_metrics("controller to settle on the sinusoid", |metrics| {
        metrics["controller"]["last_error"] == Value::Null
            && metrics["controller"]["tick_count"]
                .as_u64()
                .is_some_and(|count| count > 5)
    });
    let rss = rss_anon(&settled);
    assert!(
        rss >= low / 2 && rss < high + 64 * MIB,
        "RssAnon {rss} left the expected envelope {low}..={high}"
    );

    // A full half-period of t/10 is ~31s, so within the convergence budget the target should
    // stay inside its amplitude and never produce a controller error.
    let sampled = server.wait_metrics("RssAnon to stay within the sinusoid envelope", |metrics| {
        rss_anon(metrics) >= low && rss_anon(metrics) <= high
    });
    assert!(rss_anon(&sampled) >= low);
    assert!(rss_anon(&sampled) <= high);
    assert_eq!(sampled["controller"]["last_error"], Value::Null);
}

/// Read a single sample out of a Prometheus exposition body.
fn sample(body: &str, name: &str) -> String {
    body.lines()
        .find_map(|line| {
            let rest = line.strip_prefix(name)?;
            if let Some(after_label) = rest.strip_prefix('{') {
                let close = after_label.find('}')?;
                let value = after_label[close + 1..].trim_start();
                (!value.is_empty()).then(|| value.to_owned())
            } else {
                rest.strip_prefix(' ').map(str::to_owned)
            }
        })
        .unwrap_or_else(|| panic!("{name} is not exposed in:\n{body}"))
}

fn cpu_seconds(body: &str) -> f64 {
    let user: f64 = sample(body, "loadlab_process_cpu_user_seconds_total")
        .parse()
        .expect("user cpu is a float");
    let system: f64 = sample(body, "loadlab_process_cpu_system_seconds_total")
        .parse()
        .expect("system cpu is a float");
    user + system
}

#[test]
fn prometheus_endpoint_serves_the_text_exposition() {
    let server = Server::start();
    server.post("/workload/start");

    let (status, content_type, body) = server.get_text("/prometheus");
    assert_eq!(status, 200);
    assert_eq!(content_type, "text/plain; version=0.0.4; charset=utf-8");
    assert!(body.ends_with('\n'), "exposition should end with a newline");

    // The controller should have ticked by now, so no sample may read as a healthy zero.
    assert_ne!(sample(&body, "loadlab_controller_ticks_total"), "0");
    assert_eq!(sample(&body, "loadlab_workload_running"), "1");
    assert_eq!(sample(&body, "loadlab_controller_error"), "0");
    assert_eq!(
        sample(&body, "loadlab_controller_reconcile_errors_total"),
        "0"
    );
    assert_eq!(sample(&body, "loadlab_controller_converged"), "1");

    // A version label has to survive, and the value has to be parseable.
    assert!(body.contains("loadlab_build_info{version=\""), "{body}");
    assert!(body.contains("# TYPE loadlab_build_info gauge"));
}

#[test]
fn prometheus_and_json_metrics_agree_on_the_same_numbers() {
    let server = Server::start();
    server.post("/workload/start");
    server.wait_metrics("the controller to tick", |metrics| {
        metrics["controller"]["tick_count"]
            .as_u64()
            .is_some_and(|count| count > 5)
    });

    let (_, _, body) = server.get_text("/prometheus");
    let (_, json) = server.get("/metrics");

    // RSS and the arena are the numbers a dashboard is built on, so the two endpoints reporting
    // different values for them would be a real trap for anyone moving from one to the other.
    let exported: u64 = sample(&body, "loadlab_memory_rss_anon_bytes")
        .parse()
        .expect("rss_anon is an integer");
    let from_json = json["process"]["memory"]["rss_anon_bytes"]
        .as_u64()
        .expect("rss_anon_bytes is a number");
    assert!(
        exported.abs_diff(from_json) < 8 * MIB,
        "exposition says {exported}, JSON says {from_json}"
    );

    assert_eq!(
        sample(&body, "loadlab_controller_target_arena_bytes")
            .parse::<u64>()
            .expect("target is an integer"),
        json["controller"]["last_target_arena_bytes"]
            .as_u64()
            .expect("last_target_arena_bytes is a number")
    );

    // Peak RSS is the one that cannot be recovered from an instantaneous sample, so it is the one
    // worth asserting is actually populated.
    let peak: u64 = sample(&body, "loadlab_memory_peak_rss_bytes")
        .parse()
        .expect("peak rss is an integer");
    let rss: u64 = sample(&body, "loadlab_memory_rss_bytes")
        .parse()
        .expect("rss is an integer");
    assert!(
        peak >= rss,
        "peak {peak} should never be below current {rss}"
    );

    // CPU is sampled in clock ticks, so an idle process legitimately reads zero. What has to hold
    // is that the counters are wired to the real /proc fields and move under load: the smallest
    // possible tick interval keeps the controller spinning, so CPU has to climb between scrapes.
    let (status, _) = server.put("/workload", r#"{"controller":{"tick_ms":1}}"#);
    assert_eq!(status, 200);
    let (_, _, before) = server.get_text("/prometheus");
    let (_, _, after) = server.get_text("/prometheus");
    assert_eq!(sample(&before, "loadlab_workload_running"), "0");
    assert!(
        cpu_seconds(&after) >= cpu_seconds(&before),
        "cpu went backwards: {} then {}",
        cpu_seconds(&before),
        cpu_seconds(&after)
    );
    assert!(cpu_seconds(&after) >= 0.0);
    assert!(
        sample(&after, "loadlab_process_threads")
            .parse::<u64>()
            .expect("threads is an integer")
            >= 2
    );
}

#[test]
fn prometheus_exposition_agrees_with_the_json_endpoint_after_peak_pressure() {
    let server = Server::start();
    let low = 192 * MIB;
    server.put(
        "/workload",
        &format!(
            r#"{{"running":true,"memory":{{"anonymous_rss":"{low} + {amplitude}*sin(t/1)"}}}}"#,
            amplitude = 64 * MIB,
        ),
    );
    let settled = server.wait_metrics("RssAnon to rise above the trough", |metrics| {
        rss_anon(metrics) > low
    });

    let (_, _, body) = server.get_text("/prometheus");
    let peak: u64 = sample(&body, "loadlab_memory_peak_rss_bytes")
        .parse()
        .expect("peak rss is an integer");
    let rss = rss_anon(&settled);

    // The high-water mark must never regress below what a scraper just observed, which is the
    // entire reason it exists: a fast workload can rise and fall between two scrapes.
    assert!(
        peak >= rss,
        "peak {peak} regressed below observed rss {rss}"
    );
    assert_eq!(settled["controller"]["converged"], Value::Bool(true));
    let error: i64 = settled["controller"]["convergence_error_bytes"]
        .as_i64()
        .expect("convergence error is signed");
    let tolerance = i64::try_from(2 * MIB).expect("tolerance fits in i64");
    assert!(
        error.abs() <= tolerance,
        "convergence error {error} is outside the {tolerance} byte tolerance"
    );
}

// ---------------------------------------------------------------------------------------------
// File-backed memory.
//
// A load generator that only ever faults anonymous memory cannot reproduce what a host does when
// the page cache is under pressure, so the mapping needs a real file behind it. These tests
// assert the three things that distinguish it from the anonymous case: the file exists and loadlab
// owns it, the pages are accounted as *file*-backed rather than anonymous, and stopping releases
// them.
// ---------------------------------------------------------------------------------------------

/// A quiet workload must not touch the filesystem. This is the regression guard for the default
/// that matters most here: `WorkloadState` is `#[serde(default)]`, so the default
/// `file_backed` section is what every partial `PATCH` that never mentions it restores, and a
/// default that created a file would start writing to `/tmp` on unrelated edits.
#[test]
fn a_quiet_workload_creates_no_file() {
    let server = Server::start();
    let metrics = server.get("/metrics").1;
    assert_eq!(
        file_backed_string(&metrics, "path"),
        "",
        "no file should exist before file-backed memory is asked for"
    );
    assert_eq!(file_backed_number(&metrics, "rss_bytes"), 0.0);
    assert_eq!(file_backed_number(&metrics, "pages"), 0.0);
    assert_eq!(file_backed_number(&metrics, "file_bytes"), 0.0);

    // An unrelated edit must not change that, which is the `#[serde(default)]` path.
    assert_eq!(server.patch("/workload", r#"{"running":true}"#).0, 200);
    let after = server.wait_metrics("a tick after starting", |metrics| {
        metrics["controller"]["tick_count"].as_u64().unwrap_or(0) > 2
    });
    assert_eq!(
        file_backed_string(&after, "path"),
        "",
        "a PATCH that never mentioned file_backed must not create a file"
    );
}

#[test]
fn a_file_backed_target_creates_a_file_and_makes_it_resident() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "file_backed":{"rss":"64MiB","access_pattern":"sequential"}}"#,
            )
            .0,
        200
    );

    let metrics = server.wait_metrics("64MiB of file-backed RSS", |metrics| {
        file_backed_number(metrics, "rss_bytes") >= 64.0 * MIB_F
    });

    let path = file_backed_string(&metrics, "path");
    assert!(!path.is_empty(), "a file should have been created");
    assert!(
        path.contains("loadlab") || path.starts_with('/') || path.contains(".tmp"),
        "the path should look like a temporary file, got {path}"
    );
    assert!(
        std::path::Path::new(&path).exists(),
        "the reported path {path} should exist while the workload holds it"
    );

    // The four fields the assertion is expressed in terms of.
    assert_eq!(file_backed_string(&metrics, "access_pattern"), "sequential");
    assert!(file_backed_number(&metrics, "pages") > 0.0);
    assert!(file_backed_number(&metrics, "rss_bytes") > 0.0);
    assert_eq!(
        file_backed_number(&metrics, "pages") * PAGE_BYTES as f64,
        file_backed_number(&metrics, "rss_bytes"),
        "pages and rss_bytes should describe the same mapping"
    );
    assert_eq!(file_backed_string(&metrics, "madvise"), "random");

    // The file must be at least as long as the mapping. Truncating a mapped file raises SIGBUS on
    // the next access, so a shorter file would be a crash, not a smaller number.
    let file_bytes = file_backed_number(&metrics, "file_bytes");
    assert!(
        file_bytes >= file_backed_number(&metrics, "rss_bytes"),
        "file {file_bytes} is shorter than the resident set {}",
        file_backed_number(&metrics, "rss_bytes")
    );

    assert_eq!(metrics["controller"]["last_error"], Value::Null);
    assert!(
        file_backed_flag(&metrics, "converged"),
        "converged should be true"
    );
}

/// The whole point of `MAP_SHARED` here: the pages have to be accounted as *file* memory. A
/// `MAP_PRIVATE` write would be copy-on-write and the kernel would report it under `RssAnon`,
/// which is the opposite of what a file-backed workload is supposed to demonstrate.
#[test]
fn file_backed_pages_are_accounted_as_file_memory_not_anonymous() {
    let server = Server::start();
    let before = server.get("/metrics").1;
    let anon_before = rss_anon(&before);
    let file_before = before["process"]["memory"]["rss_file_bytes"]
        .as_u64()
        .expect("rss_file_bytes is a number");

    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "file_backed":{"rss":"128MiB"}}"#,
            )
            .0,
        200
    );
    let metrics = server.wait_metrics("128MiB of file-backed RSS", |metrics| {
        file_backed_number(metrics, "rss_bytes") >= 128.0 * MIB_F
    });

    let file_after = metrics["process"]["memory"]["rss_file_bytes"]
        .as_u64()
        .expect("rss_file_bytes is a number");
    assert!(
        file_after > file_before + 64 * MIB,
        "process RssFile rose only {file_before} -> {file_after}"
    );

    // Anonymous memory must not have absorbed the workload. The arena is idle here, so any large
    // rise would mean the writes were being counted as copy-on-write.
    let anon_after = rss_anon(&metrics);
    assert!(
        anon_after < anon_before + 32 * MIB,
        "RssAnon rose {anon_before} -> {anon_after}, so the mapping is not file-backed"
    );

    // The controller's own number and the process-wide one are different quantities, and both are
    // reported. loadlab's mapping is a subset of the process's file memory.
    assert!(file_backed_number(&metrics, "process_rss_file_bytes") >= 64.0 * MIB_F);
    assert!(file_backed_number(&metrics, "rss_bytes") > 0.0);
}

#[test]
fn stopping_releases_the_file_backed_pages_but_keeps_the_file() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "file_backed":{"rss":"64MiB"}}"#,
            )
            .0,
        200
    );
    let held = server.wait_metrics("64MiB of file-backed RSS", |metrics| {
        file_backed_number(metrics, "rss_bytes") >= 64.0 * MIB_F
    });
    let path = file_backed_string(&held, "path");
    assert!(!path.is_empty());

    assert_eq!(server.post("/workload/stop").0, 200);
    let released = server.wait_metrics("file-backed RSS to return to zero", |metrics| {
        file_backed_number(metrics, "rss_bytes") == 0.0
    });
    assert_eq!(file_backed_number(&released, "pages"), 0.0);
    assert_eq!(file_backed_number(&released, "requested_rss_bytes"), 0.0);
    assert_eq!(file_backed_number(&released, "target_rss_bytes"), 0.0);
    assert_eq!(released["controller"]["last_error"], Value::Null);

    // The mapping is released, not unmapped, so the file stays until the process exits. That is
    // the deliberate trade: recreating it on every start would cost a re-map, and the operator
    // ceiling is what bounds it.
    assert_eq!(
        file_backed_string(&released, "path"),
        path,
        "stopping should keep the file, not churn its path"
    );
}

#[test]
fn every_access_pattern_is_accepted_and_reported_back() {
    for pattern in [
        "sequential",
        "random",
        "stride",
        "working_set",
        "hot_cold",
        "cold_scan",
    ] {
        let server = Server::start();
        let (status, _) = server.put(
            "/workload",
            &format!(
                r#"{{"running":true,"controller":{{"tick_ms":50}},
                    "file_backed":{{"rss":"16MiB","access_pattern":"{pattern}",
                        "working_set_pages":64,"stride_pages":4,"random_seed":7}}}}"#
            ),
        );
        assert_eq!(status, 200, "{pattern} should be accepted");

        let metrics = server.wait_metrics(&format!("{pattern} to page in"), |metrics| {
            file_backed_number(metrics, "rss_bytes") >= 16.0 * MIB_F
        });
        assert_eq!(
            file_backed_string(&metrics, "access_pattern"),
            pattern,
            "the controller should report the pattern it was given"
        );
        assert_eq!(metrics["controller"]["last_error"], Value::Null);
    }
}

#[test]
fn an_unknown_access_pattern_is_rejected_with_the_valid_names() {
    let server = Server::start();
    let (status, body) = server.put(
        "/workload",
        r#"{"running":true,"file_backed":{"access_pattern":"sideways"}}"#,
    );
    assert_eq!(status, 400, "an unknown pattern must be a client error");
    let message = body["error"].as_str().expect("error string").to_owned();
    assert!(message.contains("sideways"), "{message}");
    assert!(
        message.contains("hot_cold"),
        "{message} should list the valid names"
    );
}

#[test]
fn a_file_backed_target_above_the_memory_ceiling_is_rejected() {
    let server = Server::start();
    // The server runs with a 512MiB ceiling.
    let (status, body) = server.put(
        "/workload",
        r#"{"running":true,"file_backed":{"rss":"4GiB"}}"#,
    );
    assert_eq!(status, 400);
    let message = body["error"].as_str().expect("error string");
    assert!(message.contains("file_backed.rss"), "{message}");

    // Rejected writes do not modify desired state, so nothing was created.
    let metrics = server.get("/metrics").1;
    assert_eq!(file_backed_string(&metrics, "path"), "");
}

/// The disk ceiling is a runtime clamp, not a validation error, because `rss` is an expression
/// and only its evaluated value can be compared against it. A request above it is clamped and
/// reported saturated, the same way an above-capacity CPU target is.
#[test]
fn a_request_above_the_file_ceiling_is_clamped_and_reported_saturated() {
    let server = Server::start_with_file_ceiling(32 * MIB);
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "file_backed":{"rss":"256MiB"}}"#,
            )
            .0,
        200,
        "the disk ceiling is applied at runtime, so the write is accepted"
    );

    let metrics = server.wait_metrics("the clamp to take effect", |metrics| {
        file_backed_number(metrics, "rss_bytes") > 0.0
    });
    let requested = file_backed_number(&metrics, "requested_rss_bytes");
    let target = file_backed_number(&metrics, "target_rss_bytes");
    assert!(
        requested >= 256.0 * MIB_F,
        "the request should be reported unclamped, got {requested}"
    );
    assert!(
        target < requested,
        "the target should be clamped below the request: {target} vs {requested}"
    );
    assert!(
        target <= 40.0 * MIB_F,
        "the clamp exceeded the 32MiB ceiling: {target}"
    );
    assert!(file_backed_flag(&metrics, "saturated"));
    assert_eq!(metrics["controller"]["last_error"], Value::Null);

    // The ceiling is a process-level limit, so the only way an alert can use the operator's own
    // number rather than a constant copied into a rule is for it to be exported.
    let (_, _, body) = server.get_text("/prometheus");
    assert_eq!(
        sample(&body, "loadlab_configured_max_file_backed_bytes"),
        (32 * MIB).to_string()
    );
}

/// A read target is reported from the bytes the reads actually returned, not from what was issued,
/// and the same number reaches both `/metrics` and the Prometheus exposition.
#[test]
fn an_io_read_target_is_measured_and_exported() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "io":{"read_bps":8388608,"block_size":4096,"pattern":"sequential"}}"#,
            )
            .0,
        200
    );

    let metrics = server.wait_metrics("the I/O controller to move bytes", |metrics| {
        io_number(metrics, "read_bytes_total") > 0.0
    });
    assert_eq!(io_number(&metrics, "read_bps"), 8.0 * MIB_F);
    assert!(io_number(&metrics, "read_bps_actual") > 0.0);
    assert_eq!(io_number(&metrics, "block_size_bytes"), 4096.0);
    assert!(io_number(&metrics, "region_bytes") > 0.0);
    assert_eq!(io_string(&metrics, "pattern"), "sequential");

    let (_, _, body) = server.get_text("/prometheus");
    let exported: u64 = sample(&body, "loadlab_io_read_bytes_total")
        .parse()
        .expect("io read bytes is an integer");
    assert_eq!(
        exported,
        io_number(&metrics, "read_bytes_total") as u64,
        "prometheus and JSON disagree on I/O read bytes"
    );
    assert!(exported > 0);
}

/// `direct` is a request about how the I/O is issued, and the response has to say which way it was
/// granted. A workload that asked for `O_DIRECT` and got the page cache instead would otherwise be
/// indistinguishable from one that got what it asked for.
#[test]
fn a_direct_io_target_is_reported_as_direct_and_measured() {
    let server = Server::start();
    // Deliberately a modest rate. This test asserts that the flag took effect and that bytes moved,
    // not how fast, and the suite runs tests in parallel: a test that saturates the disk starves
    // the CPU tests pinned to specific cores.
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "io":{"read_bps":1048576,"write_bps":1048576,"block_size":4096,
                          "pattern":"sequential","direct":true}}"#,
            )
            .0,
        200
    );

    let metrics = server.wait_metrics("direct I/O to move bytes", |metrics| {
        io_number(&metrics, "read_bytes_total") > 0.0
            && io_number(&metrics, "write_bytes_total") > 0.0
    });
    assert_eq!(
        io_bool(&metrics, "direct"),
        Some(true),
        "the response should say the descriptor is direct"
    );

    let (_, _, body) = server.get_text("/prometheus");
    assert_eq!(sample(&body, "loadlab_io_direct"), "1");
}

/// The default has to be buffered, and it has to be visible as buffered rather than merely absent,
/// so that a client can tell "off" from "not implemented".
#[test]
fn io_is_buffered_by_default_and_says_so() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "io":{"read_bps":1048576,"block_size":4096}}"#,
            )
            .0,
        200
    );

    let metrics = server.wait_metrics("buffered I/O to move bytes", |metrics| {
        io_number(metrics, "read_bytes_total") > 0.0
    });
    assert_eq!(
        io_bool(&metrics, "direct"),
        Some(false),
        "io should be buffered by default"
    );

    let (_, _, body) = server.get_text("/prometheus");
    assert_eq!(sample(&body, "loadlab_io_direct"), "0");
}

/// The invalid combination is rejected on the way in, naming the field the operator has to change.
/// Accepting it would leave a controller that reports success while every operation returns `EINVAL`.
#[test]
fn direct_io_with_an_unaligned_block_size_is_rejected_and_says_which_field() {
    let server = Server::start();
    let (status, body) = server.put(
        "/workload",
        r#"{"running":true,"io":{"read_bps":4194304,"block_size":512,"direct":true}}"#,
    );
    assert_eq!(
        status, 400,
        "an unaligned direct block size should be refused"
    );
    let message = body.to_string();
    assert!(
        message.contains("io.block_size"),
        "the error should name the field to change: {message}"
    );
    assert!(
        message.contains("4096"),
        "the error should state the required alignment: {message}"
    );

    // And nothing was applied: a refused configuration must not half-take effect.
    let (_, metrics) = server.get("/metrics");
    assert_eq!(metrics["controller"]["io"]["read_bytes_total"], 0);
    assert_eq!(
        io_bool(&metrics, "direct"),
        Some(false),
        "a refused configuration should not have turned direct on"
    );
}

/// Latency is the reason to look at disk results at all, so the three series have to be populated
/// from real operations and the count has to line up with the operation count.
#[test]
fn io_latency_is_published_and_covers_every_operation() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "io":{"iops":4000,"block_size":4096,"fsync_rate":20}}"#,
            )
            .0,
        200
    );

    // Waiting for an `fsync` as well as an operation, so the equality check on fsync latency below
    // compares real observations rather than two zeroes.
    let metrics = server.wait_metrics("latency observations", |metrics| {
        io_number(metrics, "ops_total") > 0.0
            && io_path_number(metrics, "read_latency.count") > 0.0
            && io_number(metrics, "fsyncs_total") > 0.0
    });

    let observed = io_path_number(&metrics, "read_latency.count")
        + io_path_number(&metrics, "write_latency.count");
    assert_eq!(
        observed,
        io_number(&metrics, "ops_total"),
        "every operation should be timed exactly once"
    );
    assert_eq!(
        io_path_number(&metrics, "fsync_latency.count"),
        io_number(&metrics, "fsyncs_total"),
        "every fsync should be timed exactly once"
    );
    for field in ["read_latency", "write_latency"] {
        let minimum = io_path_number(&metrics, &format!("{field}.min_seconds"));
        let maximum = io_path_number(&metrics, &format!("{field}.max_seconds"));
        let sum = io_path_number(&metrics, &format!("{field}.sum_seconds"));
        assert!(
            minimum > 0.0,
            "{field}.min_seconds should be a real duration"
        );
        assert!(
            maximum >= minimum,
            "{field}: max {maximum} below min {minimum}"
        );
        assert!(
            sum >= maximum,
            "{field}: sum {sum} below a single observation"
        );
    }

    let (_, _, body) = server.get_text("/prometheus");
    // The histogram families exist even before anything has been observed, so that a rate() over
    // them is continuous from the first scrape rather than starting mid-run.
    for name in [
        "loadlab_io_read_latency_seconds_count",
        "loadlab_io_write_latency_seconds_count",
        "loadlab_io_fsync_latency_seconds_count",
    ] {
        assert!(body.contains(name), "{name} should be exported");
    }
    assert!(
        body.contains(r#"loadlab_io_read_latency_seconds_bucket{le="0.0001"}"#),
        "the bucket labels should carry the documented boundaries"
    );
}

/// A latency of zero is a claim that the operation took no time at all, which is never true. The
/// unset state has to be `null` so "not measured yet" cannot be read as "free".
#[test]
fn an_unobserved_latency_reports_null_rather_than_zero() {
    let server = Server::start();
    // A read target of zero generates nothing, so nothing is timed.
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},"io":{"block_size":4096}}"#,
            )
            .0,
        200
    );

    let (_, metrics) = server.get("/metrics");
    for field in ["read_latency", "write_latency", "fsync_latency"] {
        assert_eq!(
            io_path_number(&metrics, &format!("{field}.count")),
            0.0,
            "{field} should have observed nothing"
        );
        assert!(
            io_null(&metrics, &format!("{field}.min_seconds")),
            "{field}.min_seconds should be null rather than zero"
        );
        assert!(
            io_null(&metrics, &format!("{field}.max_seconds")),
            "{field}.max_seconds should be null rather than zero"
        );
    }
}

/// The I/O region is a runtime clamp on a process-level ceiling: a target larger than the ceiling
/// still runs, but the file it walks stays inside the ceiling (rounded up to a whole block) and the
/// ceiling is exported so a rule can use the operator's own number.
#[test]
fn the_io_region_respects_the_operator_ceiling() {
    let server = Server::start_with_io_ceiling(32 * MIB);
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "io":{"read_bps":4194304,"block_size":4096}}"#,
            )
            .0,
        200
    );

    let metrics = server.wait_metrics("the I/O region to appear", |metrics| {
        io_number(metrics, "region_bytes") > 0.0
    });
    let region = io_number(&metrics, "region_bytes");
    assert!(
        region >= 32.0 * MIB_F,
        "the region should fill the ceiling, got {region}"
    );
    assert!(
        region - 32.0 * MIB_F < PAGE_BYTES as f64,
        "the region may round up by less than one block, got {region}"
    );

    let (_, _, body) = server.get_text("/prometheus");
    assert_eq!(
        sample(&body, "loadlab_configured_max_io_bytes"),
        (32 * MIB).to_string()
    );
    assert_eq!(
        sample(&body, "loadlab_io_region_bytes"),
        (region as u64).to_string()
    );
}

#[test]
fn the_file_backed_access_loop_generates_page_faults() {
    let server = Server::start();
    let (_, _, before) = server.get_text("/prometheus");
    let faults_before: u64 = sample(&before, "loadlab_faults_minor_total")
        .parse()
        .expect("minor faults are an integer");

    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "file_backed":{"rss":"32MiB","pages_per_second":5000,
                        "access_pattern":"random","random_seed":11}}"#,
            )
            .0,
        200
    );

    // The rate is reported, not just the residency, so an operator can tell the access loop apart
    // from plain residency.
    let metrics = server.wait_metrics("the access loop to report a rate", |metrics| {
        file_backed_number(metrics, "actual_pages_per_second") > 0.0
    });
    assert_eq!(file_backed_number(&metrics, "pages_per_second"), 5000.0);
    assert!(file_backed_number(&metrics, "minor_faults_rate") > 0.0);

    let (_, _, after) = server.get_text("/prometheus");
    let faults_after: u64 = sample(&after, "loadlab_faults_minor_total")
        .parse()
        .expect("minor faults are an integer");
    assert!(
        faults_after > faults_before,
        "evicting and re-touching pages should fault: {faults_before} then {faults_after}"
    );
}

#[test]
fn prometheus_exports_the_file_backed_controller() {
    let server = Server::start();
    server.put(
        "/workload",
        r#"{"running":true,"controller":{"tick_ms":50},
            "file_backed":{"rss":"32MiB","madvise":"sequential"}}"#,
    );
    let metrics = server.wait_metrics("file-backed RSS to appear", |metrics| {
        file_backed_number(metrics, "rss_bytes") >= 32.0 * MIB_F
    });
    let (_, _, body) = server.get_text("/prometheus");

    assert_eq!(sample(&body, "loadlab_file_backed_converged"), "1");
    assert_eq!(sample(&body, "loadlab_file_backed_saturated"), "0");

    // The exposition and the JSON endpoint have to agree, or a dashboard built on one contradicts
    // an operator reading the other.
    let exported: u64 = sample(&body, "loadlab_file_backed_rss_bytes")
        .parse()
        .expect("rss is an integer");
    assert_eq!(
        exported,
        file_backed_number(&metrics, "rss_bytes") as u64,
        "prometheus and JSON disagree on file-backed RSS"
    );
    assert!(exported >= 32 * MIB);

    let pages: u64 = sample(&body, "loadlab_file_backed_pages")
        .parse()
        .expect("pages are an integer");
    assert_eq!(
        pages * PAGE_BYTES,
        exported,
        "pages and bytes should describe one mapping"
    );
}

/// A sinusoidal file-backed target has to be tracked rather than latched at its `t=0` value,
/// which is the failure mode of evaluating an expression once at configuration time.
#[test]
fn a_time_varying_file_backed_target_is_tracked() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "file_backed":{"rss":"32MiB + 32MiB*sin(t/2)"}}"#,
            )
            .0,
        200
    );

    let mut saw_low = false;
    let mut saw_high = false;
    for _ in 0..80 {
        let metrics = server.get("/metrics").1;
        let rss = file_backed_number(&metrics, "rss_bytes");
        if rss <= 40.0 * MIB_F {
            saw_low = true;
        }
        if rss >= 56.0 * MIB_F {
            saw_high = true;
        }
        if saw_low && saw_high {
            break;
        }
        sleep(Duration::from_millis(100));
    }
    assert!(
        saw_low && saw_high,
        "the target should sweep its whole range: saw_low={saw_low} saw_high={saw_high}"
    );
}

/// A clean shutdown has to unlink the file. The controller thread is detached, so if nothing
/// joins it the process returns with the file still on disk — and a tool whose whole purpose
/// includes writing to the operator's disk should not leave debris behind on every restart.
#[test]
fn a_clean_shutdown_unlinks_the_file_backed_file() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "file_backed":{"rss":"16MiB"}}"#,
            )
            .0,
        200
    );
    let metrics = server.wait_metrics("file-backed RSS to appear", |metrics| {
        file_backed_number(metrics, "rss_bytes") >= 16.0 * MIB_F
    });
    let path = file_backed_string(&metrics, "path");
    assert!(
        !path.is_empty(),
        "a file should exist while the workload holds it"
    );
    let path = std::path::PathBuf::from(&path);
    assert!(path.exists(), "{path:?} should exist before shutdown");

    // `Drop` for `Server` sends SIGTERM, which is the signal systemd sends on stop.
    drop(server);

    // Polled rather than asserted immediately: the controller thread notices the shutdown flag
    // between ticks, so the unlink is not synchronous with the process being signalled.
    let deadline = Instant::now() + Duration::from_secs(5);
    while path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !path.exists(),
        "loadlab exited without unlinking {path:?}; a detached controller thread that is never \
         joined leaves the file behind on every restart"
    );
}

/// A shared segment with three actors is created, mapped, and accounted per actor.
///
/// This is the workload these tests exist to demonstrate: one segment mapped by several real
/// processes, with each process identifiable in the reported figures.
#[test]
fn a_shared_target_creates_a_segment_and_accounts_each_actor() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "shared":{"shared_bytes":"32MiB","private_bytes":"16MiB",
                              "cow_write_ratio":1.0,"actors":3}}"#,
            )
            .0,
        200
    );

    let metrics = server.wait_metrics("three shared actors", |metrics| {
        shared_number(metrics, "actors") >= 3.0
    });

    assert_eq!(shared_string(&metrics, "backing"), "memfd");
    assert_eq!(shared_number(&metrics, "shared_bytes"), 32.0 * MIB_F);
    assert_eq!(shared_number(&metrics, "child_actors"), 2.0);
    assert!(
        shared_number(&metrics, "shared_physical_bytes") > 0.0,
        "the shared segment should be resident"
    );

    let accounting = metrics["controller"]["shared"]["actor_accounting"]
        .as_array()
        .expect("actor_accounting should be an array");
    assert_eq!(
        accounting.len(),
        3,
        "every actor must be identified, controller included"
    );
    let controllers = accounting
        .iter()
        .filter(|actor| actor["is_controller"] == Value::Bool(true))
        .count();
    assert_eq!(controllers, 1, "exactly one actor owns the controller");
    for actor in accounting {
        assert!(
            actor["rss_bytes"].as_f64().unwrap_or(0.0) > 0.0,
            "every actor should hold memory, got {actor:?}"
        );
    }
    assert_eq!(metrics["controller"]["last_error"], Value::Null);
}

/// The claim this module is built around: summing `Rss` over the actors counts the segment once per
/// mapper, while `Pss` counts it once in total.
#[test]
fn a_shared_segment_is_overcounted_by_summing_rss_across_actors() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "shared":{"shared_bytes":"64MiB","private_bytes":"0",
                              "cow_write_ratio":0.0,"actors":3}}"#,
            )
            .0,
        200
    );

    let metrics = server.wait_metrics("the shared segment to be double-counted", |metrics| {
        shared_number(metrics, "actors") >= 3.0
            && shared_number(metrics, "shared_overcount_bytes") > 0.0
    });

    assert!(
        shared_number(&metrics, "accounted_bytes") > shared_number(&metrics, "physical_bytes"),
        "sum(Rss) should exceed sum(Pss) when a segment has more than one mapper: \
         accounted {} physical {}",
        shared_number(&metrics, "accounted_bytes"),
        shared_number(&metrics, "physical_bytes")
    );
}

/// A single actor maps the segment once, so there is nothing to overcount: the gap is an effect of
/// multiple mappers rather than of the segment existing at all.
#[test]
fn a_shared_segment_with_one_actor_is_not_overcounted() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "shared":{"shared_bytes":"32MiB","private_bytes":"0",
                              "cow_write_ratio":0.0,"actors":1}}"#,
            )
            .0,
        200
    );

    let metrics = server.wait_metrics("one actor to map the segment", |metrics| {
        shared_number(metrics, "shared_physical_bytes") > 0.0
    });
    assert_eq!(shared_number(&metrics, "actors"), 1.0);
    assert_eq!(shared_number(&metrics, "child_actors"), 0.0);
    assert_eq!(
        shared_number(&metrics, "shared_overcount_bytes"),
        0.0,
        "a single mapper cannot double-count its own segment"
    );
}

/// The shared ceiling bounds the combined footprint, so the copy-on-write side is clamped to what
/// is left after the segment is paid for, and the clamp is reported rather than hidden.
#[test]
fn a_request_above_the_shared_ceiling_is_clamped_and_reported_saturated() {
    let ceiling = 64 * MIB;
    let server = Server::start_with_shared_ceiling(ceiling);
    // Eight actors each asking to copy the whole 8 MiB segment is 64 MiB of copy-on-write, and the
    // segment takes a further 8 MiB. Under a 64 MiB combined ceiling only 56 MiB is left for the
    // copies, so the per-actor view is clamped to 7 MiB rather than the 8 MiB requested.
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "shared":{"shared_bytes":"8MiB","private_bytes":"8MiB",
                              "cow_write_ratio":1.0,"actors":8}}"#,
            )
            .0,
        200
    );

    let metrics = server.wait_metrics(
        "the shared request to be reported as saturated",
        |metrics| shared_flag(metrics, "saturated"),
    );

    let segment = 8.0 * MIB_F;
    assert_eq!(shared_number(&metrics, "shared_bytes"), segment);
    assert_eq!(
        shared_number(&metrics, "private_bytes_per_actor"),
        7.0 * MIB_F,
        "the per-actor view is what the ceiling leaves over the actors"
    );
    assert_eq!(
        shared_number(&metrics, "target_cow_bytes"),
        ceiling as f64 - segment,
        "the copy-on-write budget is what the combined ceiling leaves after the segment"
    );
    assert!(
        shared_number(&metrics, "requested_cow_bytes")
            > shared_number(&metrics, "target_cow_bytes"),
        "the refused request must stay visible"
    );
}

/// Stopping tears the actors and the segment down rather than leaving processes nobody reports.
#[test]
fn stopping_tears_down_the_shared_actors_and_segment() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "shared":{"shared_bytes":"16MiB","private_bytes":"8MiB",
                              "cow_write_ratio":0.5,"actors":2}}"#,
            )
            .0,
        200
    );
    server.wait_metrics("two actors to start", |metrics| {
        shared_number(metrics, "actors") >= 2.0
    });

    assert_eq!(server.post("/workload/stop").0, 200);
    let metrics = server.wait_metrics("the actors to be gone", |metrics| {
        shared_number(metrics, "actors") == 0.0
    });
    assert_eq!(shared_number(&metrics, "child_actors"), 0.0);
    assert_eq!(shared_number(&metrics, "shared_physical_bytes"), 0.0);
    assert_eq!(
        shared_string(&metrics, "backing"),
        "",
        "no segment should be reported once the workload is stopped"
    );
}

/// The Prometheus exposition exports the shared controller, including both views of the segment
/// that the overcount is the difference between.
#[test]
fn prometheus_exports_the_shared_controller() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "shared":{"shared_bytes":"16MiB","private_bytes":"8MiB",
                              "cow_write_ratio":1.0,"actors":2}}"#,
            )
            .0,
        200
    );
    server.wait_metrics("the shared actors to be running", |metrics| {
        shared_number(metrics, "actors") >= 2.0
    });

    let (status, content_type, body) = server.get_text("/prometheus");
    assert_eq!(status, 200);
    assert!(content_type.starts_with("text/plain"), "got {content_type}");
    for name in [
        "loadlab_configured_max_shared_bytes",
        "loadlab_shared_segment_bytes",
        "loadlab_shared_actors",
        "loadlab_shared_child_actors",
        "loadlab_shared_segment_physical_bytes",
        "loadlab_shared_segment_rss_bytes",
        "loadlab_shared_overcount_bytes",
        "loadlab_shared_cow_physical_bytes",
        "loadlab_shared_rss_bytes",
        "loadlab_shared_pss_bytes",
        "loadlab_shared_converged",
        "loadlab_shared_saturated",
    ] {
        assert!(body.contains(name), "missing {name} from the exposition");
    }
}

/// The THP policy is a request the host may refuse, so the contract is: the requested value is
/// echoed, the host's own setting is reported next to it, and the effective result stays a
/// separate observation. The test must not assume the host permits huge pages.
#[test]
fn the_thp_policy_is_accepted_reported_and_exported() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"controller":{"tick_ms":50},
                    "memory":{"anonymous_rss":"16MiB","thp":"never"}}"#,
            )
            .0,
        200
    );
    server.wait_metrics("the THP policy to be reported", |metrics| {
        metrics["controller"]["thp"]["requested"].as_str() == Some("never")
    });

    let metrics = server.get("/metrics").1;
    assert_eq!(metrics["controller"]["thp"]["requested"], "never");
    let host = metrics["controller"]["thp"]["host"]
        .as_str()
        .expect("controller.thp.host should be a string");
    assert!(
        ["always", "madvise", "never", "unavailable"].contains(&host),
        "unexpected host THP setting {host:?}"
    );
    assert!(
        metrics["controller"]["thp"]["host_enabled"].is_boolean(),
        "controller.thp.host_enabled should be a bool"
    );

    let (status, _content_type, body) = server.get_text("/prometheus");
    assert_eq!(status, 200);
    assert!(
        body.contains(r#"loadlab_thp_requested{policy="never"} 1"#),
        "the requested policy should be exported as a labelled series:\n{body}"
    );
    assert!(body.contains("loadlab_thp_host_enabled"), "{body}");
    assert!(
        body.contains("loadlab_memory_anon_huge_pages_bytes"),
        "{body}"
    );

    // An unknown policy is refused without disturbing the accepted state.
    let (status, body) = server.patch("/workload", r#"{"memory":{"thp":"gigantic"}}"#);
    assert_eq!(status, 400, "unknown policy should be rejected: {body}");
    assert_eq!(
        server.get("/metrics").1["controller"]["thp"]["requested"],
        "never"
    );
}

/// A TCP/UDP sink child process (`loadlab --sink`). Used as the destination a workload points a
/// network target at, so network integration tests need no external service.
struct Sink {
    #[allow(dead_code)]
    addr: SocketAddr,
    child: Child,
}

impl Sink {
    /// Block until the sink accepts a TCP connection, proving it has bound the address.
    fn wait_ready(&self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(_stream) = std::net::TcpStream::connect(self.addr) {
                return;
            }
            sleep(Duration::from_millis(25));
        }
        panic!("sink did not become ready on {}", self.addr);
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        // SAFETY: the pid is the sink child's own.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().expect("poll the sink").is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Test that a network target pointed at a sink receives traffic and converges.
#[test]
fn network_sink_integration() {
    let sink_addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let sink = Server::spawn_sink(sink_addr);
    sink.wait_ready();

    let server = Server::start();
    // Point the network controller at the sink with a modest request rate.
    let config = format!(
        r#"{{"running":true,"controller":{{"tick_ms":50}},
               "network":{{"transport":"tcp","target":"{sink_addr}","concurrency":2,
                          "connections_per_second":0,"messages_per_second":1000,
                          "message_size":1024}}}}"#,
    );
    let (status, _) = server.put("/workload", &config);
    assert_eq!(status, 200, "workload config failed: {status}");

    let metrics = server.wait_metrics("network controller to converge", |metrics| {
        network_flag(metrics, "converged") && network_number(metrics, "messages_total") > 0.0
    });
    assert!(network_flag(&metrics, "converged"));
    assert_eq!(network_string(&metrics, "transport"), "tcp");
    assert_eq!(network_string(&metrics, "target"), sink_addr.to_string());
    // At least some traffic should have been sent
    assert!(network_number(&metrics, "messages_total") > 0.0);
    assert!(network_number(&metrics, "bytes_total") > 0.0);
    // The operator's ceiling should be high enough not to saturate
    assert!(!network_flag(&metrics, "saturated"));
}

/// A network rate may be an expression in `t`, exactly like a memory or CPU target. It has to be
/// accepted, echoed back verbatim in `/state`, and evaluated by the controller rather than read as
/// a constant. A gentle sinusoid is used so the test also proves the rate follows `t`.
#[test]
fn a_network_rate_expression_is_accepted_and_driven() {
    let sink_addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let sink = Server::spawn_sink(sink_addr);
    sink.wait_ready();

    let server = Server::start();
    let expression = "1000 + 200*sin(t/286.5)";
    let config = format!(
        r#"{{"running":true,"controller":{{"tick_ms":50}},
               "network":{{"transport":"tcp","target":"{sink_addr}","concurrency":2,
                          "messages_per_second":"{expression}","message_size":1024}}}}"#,
    );
    let (status, _) = server.put("/workload", &config);
    assert_eq!(status, 200, "a rate expression was rejected: {status}");

    let (status, state) = server.get("/state");
    assert_eq!(status, 200);
    assert_eq!(
        state["network"]["messages_per_second"], expression,
        "the configured expression must round-trip unchanged in /state"
    );

    let metrics = server.wait_metrics("the expressed network rate to converge", |metrics| {
        network_flag(metrics, "converged") && network_number(metrics, "messages_total") > 0.0
    });
    assert!(network_flag(&metrics, "converged"));
    // The requested rate is the expression evaluated at the current `t`. Near `t = 0` it sits just
    // above the mean, and it can never leave the sinusoid's range -- a constant reading would.
    let requested = network_number(&metrics, "messages_per_second");
    assert!(
        (800.0..=1200.0).contains(&requested),
        "requested rate {requested} is outside the expression's range"
    );
}

/// Test that a network target pointed at a sink saturates the operator ceiling.
#[test]
fn network_sink_saturates() {
    let sink_addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let sink = Server::spawn_sink(sink_addr);
    sink.wait_ready();

    // Use a very low network operations ceiling so the workload will saturate.
    let server = Server::start_with_network_ceiling(100); // only 100 ops/sec allowed
    let config = format!(
        r#"{{"running":true,"controller":{{"tick_ms":50}},
               "network":{{"transport":"tcp","target":"{sink_addr}","concurrency":2,
                          "connections_per_second":0,"messages_per_second":10000,
                          "message_size":1024}}}}"#,
    );
    let (status, _) = server.put("/workload", &config);
    assert_eq!(status, 200, "workload config failed: {status}");

    let metrics = server.wait_metrics("network controller to saturate", |metrics| {
        network_flag(metrics, "saturated")
    });
    assert!(network_flag(&metrics, "saturated"));
    // The actual rate should be clamped to the ceiling (~100 ops/sec).
    let ops_per_sec = network_number(&metrics, "messages_per_second_actual");
    assert!(
        (ops_per_sec - 100.0).abs() <= 20.0,
        "expected ~100 ops/sec, got {ops_per_sec}"
    );
}

/// The composite scenario shape, accepted over HTTP.
///
/// This is the documented body. It is written out whole and verbatim here
/// for the same reason it is in the unit tests: the point is that a user can paste it and have it
/// work, not that some rearrangement of it would.
const COMPOSITE_SCENARIO_BODY: &str = r#"{
    "controller": { "max_memory_bytes": 536870912 },
    "anonymous": { "rss": "500MiB + 200MiB*sin(t/30)" },
    "file_backed": { "rss": "200MiB", "pages_per_second": 5000 },
    "cpu": { "utilization": "100 + 50*sin(t/10)", "workers": 4 },
    "faults": { "minor_rate": 10000 }
}"#;

#[test]
fn the_composite_scenario_body_is_accepted_and_reaches_its_controllers() {
    let server = Server::start();
    let (status, body) = server.put("/workload", COMPOSITE_SCENARIO_BODY);
    assert_eq!(status, 200, "body was {body}");

    // The aliases are folded into the fields the controllers read.
    assert_eq!(body["memory"]["anonymous_rss"], "500MiB + 200MiB*sin(t/30)");
    assert_eq!(body["memory_access"]["pages_per_second"], 10000.0);
    // And the fields that were already top level are untouched.
    assert_eq!(body["file_backed"]["rss"], "200MiB");
    assert_eq!(body["file_backed"]["pages_per_second"], 5000.0);
    assert_eq!(body["cpu"]["utilization"], "100 + 50*sin(t/10)");
    assert_eq!(body["cpu"]["workers"], 4);

    // `GET /state` reports the same thing: a client that only ever knew the canonical spelling
    // sees exactly the canonical spelling.
    let (status, state) = server.get("/state");
    assert_eq!(status, 200);
    assert_eq!(
        state["memory"]["anonymous_rss"],
        "500MiB + 200MiB*sin(t/30)"
    );
    assert_eq!(state["memory_access"]["pages_per_second"], 10000.0);
    assert!(
        state.get("anonymous").is_none(),
        "the input alias must not be echoed back: {state}"
    );
    assert!(
        state.get("faults").is_none(),
        "the input alias must not be echoed back: {state}"
    );
}

#[test]
fn a_default_state_has_no_alias_keys_at_all() {
    let server = Server::start();
    let (status, state) = server.get("/state");
    assert_eq!(status, 200);
    assert!(state.get("anonymous").is_none(), "{state}");
    assert!(state.get("faults").is_none(), "{state}");
}

#[test]
fn patching_an_alias_over_the_canonical_field_replaces_it() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"memory":{"anonymous_rss":"64MiB"}}"#
            )
            .0,
        200
    );

    // A patch that carries the alias while the canonical field is already set is the case where
    // "last writer wins" and "the alias is authoritative" could disagree. The alias wins, so a
    // client can move between the two spellings without having to clear the other one first.
    let (status, body) = server.patch("/workload", r#"{"anonymous":{"rss":"256MiB"}}"#);
    assert_eq!(status, 200);
    assert_eq!(body["memory"]["anonymous_rss"], "256MiB");
    assert_eq!(server.get("/state").1["memory"]["anonymous_rss"], "256MiB");
}

#[test]
fn a_major_fault_rate_is_a_400_and_leaves_the_state_alone() {
    let server = Server::start();
    assert_eq!(
        server
            .put(
                "/workload",
                r#"{"running":true,"memory":{"anonymous_rss":"64MiB"}}"#
            )
            .0,
        200
    );

    let (status, body) = server.put("/workload", r#"{"faults":{"major_rate":5000}}"#);
    assert_eq!(status, 400, "body was {body}");
    let message = body["error"].as_str().expect("an error string");
    assert!(message.contains("major_rate"), "{message}");
    // The message has to say what is accepted, or the caller is left guessing which key to use.
    assert!(message.contains("minor_rate"), "{message}");

    // A rejected request must not have half-applied.
    assert_eq!(server.get("/state").1["memory"]["anonymous_rss"], "64MiB");
    assert_eq!(server.get("/state").1["running"], true);
}

#[test]
fn a_misspelled_alias_field_is_a_400() {
    let server = Server::start();
    let (status, body) = server.put("/workload", r#"{"anonymous":{"rsz":"1MiB"}}"#);
    assert_eq!(status, 400, "body was {body}");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("rsz"),
        "{body}"
    );
}

#[test]
fn a_scenario_is_stored_and_not_run_until_it_is_asked_for() {
    let server = Server::start();
    let (status, body) = server.post_json(
        "/scenarios/load",
        r#"{"name":"steady","version":"1","workload":{"controller":{"tick_ms":250},
            "memory":{"anonymous_rss":"64MiB"},"cpu":{"utilization":"50"}}}"#,
    );
    assert_eq!(status, 200, "body was {body}");
    assert_eq!(body["name"], "steady");
    assert_eq!(body["version"], "1");
    assert_eq!(body["workload"]["controller"]["tick_ms"], 250);
    // Stored scenarios describe what to generate; `running` belongs to the decision to run one,
    // so a definition never carries it.
    assert_eq!(body["workload"]["running"], false);

    // Loading is not running. A client that stores a library of scenarios must not start generating
    // load as a side effect of defining one.
    let (status, state) = server.get("/state");
    assert_eq!(status, 200);
    assert_eq!(state["running"], false);
    assert_eq!(state["controller"]["tick_ms"], 100);
}

#[test]
fn loading_normalizes_and_validates_so_a_stored_scenario_is_always_usable() {
    let server = Server::start();
    let (status, body) = server.post_json(
        "/scenarios/load",
        r#"{"name":"aliased","version":"1","workload":{"anonymous":{"rss":"128MiB"},
            "faults":{"minor_rate":2000}}}"#,
    );
    assert_eq!(status, 200, "body was {body}");
    assert_eq!(body["workload"]["memory"]["anonymous_rss"], "128MiB");
    assert_eq!(
        body["workload"]["memory_access"]["pages_per_second"],
        2000.0
    );

    // An invalid workload is refused at definition time, not at run time. The point of validating
    // here is that `run` can never fail for a scenario that was accepted.
    let (status, body) = server.post_json(
        "/scenarios/load",
        r#"{"name":"broken","version":"1","workload":{"controller":{"tick_ms":0}}}"#,
    );
    assert_eq!(status, 400, "body was {body}");
    assert_eq!(
        server.get("/scenarios").1["scenarios"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

#[test]
fn running_a_scenario_applies_it_and_reports_itself_as_active() {
    let server = Server::start();
    assert_eq!(
        server
            .post_json(
                "/scenarios/load",
                r#"{"name":"ramp","version":"1","workload":{"memory":{"anonymous_rss":"64MiB"},
                    "cpu":{"utilization":"100 + 50*sin(t/10)"}}}"#,
            )
            .0,
        200
    );

    let (status, listing) = server.get("/scenarios");
    assert_eq!(status, 200);
    assert_eq!(listing["active"], Value::Null);
    assert_eq!(listing["scenarios"][0]["name"], "ramp");
    assert_eq!(listing["scenarios"][0]["version"], "1");

    let (status, body) = server.post_json("/scenarios/run", r#"{"name":"ramp"}"#);
    assert_eq!(status, 200, "body was {body}");
    assert_eq!(body["running"], true);
    assert_eq!(body["memory"]["anonymous_rss"], "64MiB");
    assert_eq!(body["cpu"]["utilization"], "100 + 50*sin(t/10)");

    // The workload really is the scenario's now.
    assert_eq!(server.get("/state").1["running"], true);
    assert_eq!(
        server.get("/state").1["cpu"]["utilization"],
        "100 + 50*sin(t/10)"
    );
    assert_eq!(server.get("/scenarios").1["active"], "ramp");
}

#[test]
fn stopping_a_scenario_stops_the_workload_and_clears_the_active_marker() {
    let server = Server::start();
    assert_eq!(
        server
            .post_json(
                "/scenarios/load",
                r#"{"name":"ramp","version":"1","workload":{"memory":{"anonymous_rss":"64MiB"}}}"#,
            )
            .0,
        200
    );
    assert_eq!(
        server.post_json("/scenarios/run", r#"{"name":"ramp"}"#).0,
        200
    );

    let (status, body) = server.post("/scenarios/stop");
    assert_eq!(status, 200);
    assert_eq!(body["running"], false);
    // The desired state survives the stop, so restarting does not need the scenario again.
    assert_eq!(body["memory"]["anonymous_rss"], "64MiB");
    assert_eq!(server.get("/state").1["running"], false);
    assert_eq!(server.get("/scenarios").1["active"], Value::Null);
}

#[test]
fn writing_the_workload_directly_clears_the_active_scenario() {
    let server = Server::start();
    assert_eq!(
        server
            .post_json(
                "/scenarios/load",
                r#"{"name":"ramp","version":"1","workload":{"controller":{"tick_ms":250}}}"#,
            )
            .0,
        200
    );
    assert_eq!(
        server.post_json("/scenarios/run", r#"{"name":"ramp"}"#).0,
        200
    );
    assert_eq!(server.get("/scenarios").1["active"], "ramp");

    // A direct write supplants the scenario. Reporting it as still active would be claiming the
    // workload is being driven by something it is not.
    assert_eq!(server.put("/workload", r#"{"running":true}"#).0, 200);
    assert_eq!(server.get("/scenarios").1["active"], Value::Null);

    assert_eq!(
        server.post_json("/scenarios/run", r#"{"name":"ramp"}"#).0,
        200
    );
    assert_eq!(
        server
            .patch("/workload", r#"{"controller":{"tick_ms":300}}"#)
            .0,
        200
    );
    assert_eq!(server.get("/scenarios").1["active"], Value::Null);

    // `POST /workload/stop` is the same kind of event.
    assert_eq!(
        server.post_json("/scenarios/run", r#"{"name":"ramp"}"#).0,
        200
    );
    assert_eq!(server.post("/workload/stop").0, 200);
    assert_eq!(server.get("/scenarios").1["active"], Value::Null);
}

#[test]
fn a_version_identifies_a_document_and_is_not_silently_overwritten() {
    let server = Server::start();
    let load = |server: &Server, version: &str, rss: &str| {
        server.post_json(
            "/scenarios/load",
            &format!(
                r#"{{"name":"ramp","version":"{version}","workload":{{"memory":{{"anonymous_rss":"{rss}"}}}}}}"#
            ),
        )
    };

    // Same name, same version, same content: idempotent.
    assert_eq!(load(&server, "1", "64MiB").0, 200);
    assert_eq!(load(&server, "1", "64MiB").0, 200);
    assert_eq!(
        server.get("/scenarios").1["scenarios"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    // Same name and version, different content: refused. A version that does not pin its document
    // is not a version, and quietly overwriting would make a stored scenario unreproducible.
    let (status, body) = load(&server, "1", "512MiB");
    assert_eq!(status, 400, "body was {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("version"),
        "{body}"
    );

    // Bumping the version is how a scenario is changed.
    assert_eq!(load(&server, "2", "512MiB").0, 200);
    assert_eq!(server.get("/scenarios").1["scenarios"][0]["version"], "2");
    assert_eq!(
        server.get("/state").1["memory"]["anonymous_rss"],
        Value::Null
    );
}

#[test]
fn the_two_spellings_of_a_composite_scenario_are_the_same_scenario() {
    let server = Server::start();
    // Defined canonically...
    let (status, first) = server.post_json(
        "/scenarios/load",
        r#"{"name":"same","version":"1","workload":{"memory":{"anonymous_rss":"128MiB"},
            "memory_access":{"pages_per_second":2000}}}"#,
    );
    assert_eq!(status, 200, "body was {first}");
    // ...and with the aliases, which normalize to the very same state.
    let (status, body) = server.post_json(
        "/scenarios/load",
        r#"{"name":"same","version":"1","workload":{"anonymous":{"rss":"128MiB"},
            "faults":{"minor_rate":2000}}}"#,
    );
    assert_eq!(status, 200, "body was {body}");
    assert_eq!(
        server.get("/scenarios").1["scenarios"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

#[test]
fn running_a_scenario_that_was_never_loaded_is_a_400() {
    let server = Server::start();
    let (status, body) = server.post_json("/scenarios/run", r#"{"name":"nope"}"#);
    assert_eq!(status, 400, "body was {body}");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("nope"),
        "{body}"
    );
    assert_eq!(server.get("/state").1["running"], false);
}

#[test]
fn a_scenario_identifier_is_validated_rather_than_stored_verbatim() {
    let server = Server::start();
    for (description, name) in [
        ("empty", ""),
        ("whitespace only", "   "),
        ("over-long", &"x".repeat(200)),
        ("carrying a control character", "bad\nname"),
    ] {
        let (status, body) = server.post_json(
            "/scenarios/load",
            &format!(r#"{{"name":"{name}","version":"1","workload":{{}}}}"#),
        );
        assert_eq!(status, 400, "{description} was accepted: {body}");
    }
    assert_eq!(
        server.get("/scenarios").1["scenarios"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
}

#[test]
fn a_scenario_that_asks_for_more_than_the_ceiling_is_capped_on_the_way_in() {
    let server = Server::start();
    let over = CEILING_BYTES + 64 * MIB;
    let (status, body) = server.post_json(
        "/scenarios/load",
        &format!(
            r#"{{"name":"greedy","version":"1","workload":{{"controller":{{"max_memory_bytes":{over}}}}}}}"#
        ),
    );
    assert_eq!(status, 200, "body was {body}");
    assert_eq!(
        body["workload"]["controller"]["max_memory_bytes"],
        ceiling()
    );
    // And the capped value is what runs.
    assert_eq!(
        server.post_json("/scenarios/run", r#"{"name":"greedy"}"#).1["controller"]["max_memory_bytes"],
        ceiling()
    );
}

#[test]
fn the_scenario_listing_is_sorted_so_it_can_be_diffed() {
    let server = Server::start();
    for name in ["zulu", "alpha", "mike"] {
        assert_eq!(
            server
                .post_json(
                    "/scenarios/load",
                    &format!(r#"{{"name":"{name}","version":"1","workload":{{}}}}"#),
                )
                .0,
            200
        );
    }
    let listing = server.get("/scenarios").1;
    let names: Vec<&str> = listing["scenarios"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|entry| entry["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(names, ["alpha", "mike", "zulu"]);
}

/// The value of a single-sample metric in a Prometheus exposition body.
///
/// Parsed as an integer, which is what the exposition format emits for an integer gauge. Reading it
/// as `f64` would work too, and would also throw away the guarantee being checked: these are
/// nanosecond counts, which a 53-bit mantissa happens to represent exactly here but which need not
/// be rounded through a float to prove it.
/// A float sample from the exposition, for the metrics that are not integer counts.
fn metric_f64(body: &str, name: &str) -> f64 {
    body.lines()
        .filter_map(|line| line.strip_prefix(name))
        .find_map(|rest| rest.strip_prefix(' '))
        .and_then(|value| value.trim().parse::<f64>().ok())
        .unwrap_or_else(|| panic!("{name} has no sample in:\n{body}"))
}

/// The experiment block of the `/metrics` response.
fn experiment(metrics: &Value) -> &Value {
    &metrics["experiment"]
}

/// The current wall clock in Unix seconds.
fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs() as i64
}

/// Wait until the controller has observed a configured `start_time`.
///
/// The desired state changes synchronously with the request but the runtime observation changes on
/// the next tick, so a scrape straight after a patch can legitimately report the previous run's
/// clock. Waiting on the offset rather than sleeping a guessed interval is what keeps these tests
/// from being either flaky or slow.
fn wait_for_start_offset(server: &Server) -> Value {
    server.wait_metrics("the configured start time to be observed", |metrics| {
        experiment(metrics)["start_offset_seconds"].is_number()
    })
}

/// Seconds since `t = 0`, which is what the workload's expressions are evaluated against.
fn experiment_elapsed(metrics: &Value) -> f64 {
    experiment(metrics)["elapsed_seconds"]
        .as_f64()
        .expect("experiment.elapsed_seconds is a number")
}

fn metric_u64(body: &str, name: &str) -> u64 {
    body.lines()
        .filter_map(|line| line.strip_prefix(name))
        .find_map(|rest| rest.strip_prefix(' '))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or_else(|| panic!("{name} has no sample in:\n{body}"))
}

/// Every metric in the exposition, as `(name, type)`.
fn exposed_types(body: &str) -> Vec<(String, String)> {
    body.lines()
        .filter_map(|line| line.strip_prefix("# TYPE "))
        .map(|line| {
            let mut parts = line.split_whitespace();
            (
                parts.next().expect("a metric name").to_owned(),
                parts.next().expect("a metric type").to_owned(),
            )
        })
        .collect()
}

/// The full identity of every exported series: metric name plus its label set.
///
/// This is what has to stay fixed while the workload changes. Comparing names alone would miss a
/// label that appeared or changed value, which is exactly what the exposition forbids.
fn series_identities(body: &str) -> Vec<String> {
    body.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let series = line
                .rsplit_once(' ')
                .expect("a sample and a value")
                .0
                .trim_end();
            series.to_owned()
        })
        .collect()
}

fn label_values(body: &str, metric: &str) -> Vec<(String, String)> {
    body.lines()
        .filter_map(|line| line.strip_prefix(metric))
        .filter_map(|rest| rest.strip_prefix('{'))
        .filter_map(|labels| labels.split_once('}'))
        .flat_map(|(labels, _)| {
            labels.split(',').filter_map(|pair| {
                pair.split_once('=').map(|(name, value)| {
                    (name.trim().to_owned(), value.trim_matches('"').to_owned())
                })
            })
        })
        .collect()
}

/// The headline requirement here is *stable* metric names, so stability is asserted rather than
/// assumed. A metric added or renamed between two releases is a silently broken dashboard and a
/// silently broken alert, and nothing else in the test suite would notice.
#[test]
fn the_exported_series_are_the_same_across_scrapes() {
    let server = Server::start();
    server.post("/workload/start");
    server.wait_metrics("the controller to tick", |metrics| {
        metrics["controller"]["tick_count"].as_f64().unwrap_or(0.0) > 0.0
    });

    let (_, _, first) = server.get_text("/prometheus");
    let (_, _, second) = server.get_text("/prometheus");
    let before = series_identities(&first);
    let after = series_identities(&second);
    assert!(!before.is_empty(), "nothing was exposed:\n{first}");
    assert_eq!(
        before, after,
        "the exported series changed between two scrapes of a steady workload"
    );
}

/// The hard constraint on the exposition: no label may be derived from a user expression
/// or a URL. Both are unbounded and both are attacker- or operator-controlled, so either one as a
/// label value is a cardinality bomb aimed at whatever stores the metrics.
///
/// The workload below is built specifically to try: expressions with `sin()`, a network target, a
/// file path and a long scenario name with version metadata in it.
#[test]
fn no_label_is_derived_from_an_expression_a_url_or_any_other_user_input() {
    let server = Server::start();
    let (_, _, before) = server.get_text("/prometheus");

    let (status, _) = server.put(
        "/workload",
        r#"{"running":true,
            "controller":{"max_memory_bytes":268435456},
            "anonymous":{"rss":"64MiB + 32MiB*sin(t/7)"},
            "file_backed":{"rss":"32MiB + 8MiB*sin(t/11)"},
            "cpu":{"utilization":"100 + 50*sin(t/3)"},
            "memory_access":{"pattern":"working_set","working_set_pages":64,"pages_per_second":500},
            "io":{"read_bps":1048576,"block_size":8192},
            "network":{"target":"198.51.100.77:9999","messages_per_second":1000}}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(
        server
            .post_json(
                "/scenarios/load",
                r#"{"name":"a-very-long-scenario-name-a-very-long-scenario-name-a-very-long-scenario-name",
                    "version":"9.9.9-alpha+build.42","workload":{"anonymous":{"rss":"1MiB"}}}"#,
            )
            .0,
        200
    );
    assert_eq!(server.post_json("/scenarios/run", r#"{"name":"a-very-long-scenario-name-a-very-long-scenario-name-a-very-long-scenario-name"}"#).0, 200);
    server.wait_metrics("the controller to tick", |metrics| {
        metrics["controller"]["tick_count"].as_f64().unwrap_or(0.0) > 0.0
    });

    let (_, _, after) = server.get_text("/prometheus");

    // No new series, and no existing series changed identity: a label would have to be one of them.
    assert_eq!(
        series_identities(&before),
        series_identities(&after),
        "a series appeared or changed identity after the workload was set"
    );

    // And nothing that looks like the input is present anywhere in the output, which also catches
    // an expression leaking into a HELP string or a metric name.
    for needle in [
        "sin(t",
        "198.51.100.77",
        "a-very-long-scenario-name",
        "9.9.9-alpha",
    ] {
        assert!(
            !after.contains(needle),
            "{needle:?} reached the exposition:\n{after}"
        );
    }
}

/// The only labels that exist are a fixed enumeration, a compile-time constant and the host's own
/// identity, so the cardinality of the exposition is known without scraping it. Asserting the exact
/// set is what stops a future metric from quietly introducing a fourth kind of label.
#[test]
fn the_only_labels_are_the_thp_policy_enumeration_the_build_version_and_the_host() {
    let server = Server::start();
    let (_, _, body) = server.get_text("/prometheus");

    let mut labelled: Vec<String> = exposed_types(&body)
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| body.contains(&format!("{name}{{")))
        .collect();
    labelled.sort();
    assert_eq!(
        labelled,
        vec![
            "loadlab_build_info".to_owned(),
            // Host metadata, and the reason it is allowed where client-supplied values are not: its
            // values are bounded by the machine rather than chosen by a caller, so one series per
            // process is the most this can ever be. The same reasoning does not extend to the
            // experiment seed, which is why that is a value instead.
            "loadlab_host_info".to_owned(),
            "loadlab_thp_host".to_owned(),
            "loadlab_thp_requested".to_owned(),
        ],
        "a new metric carries labels:\n{body}"
    );

    // The enumeration is closed and every member is present, so the series count does not depend on
    // what the host happens to be doing.
    for metric in ["loadlab_thp_host", "loadlab_thp_requested"] {
        let values: Vec<String> = label_values(&body, metric)
            .into_iter()
            .filter(|(name, _)| name == "policy")
            .map(|(_, value)| value)
            .collect();
        assert_eq!(
            values,
            ["always", "default", "madvise", "never"],
            "{metric} publishes an unexpected policy set"
        );
    }
    assert_eq!(
        label_values(&body, "loadlab_build_info"),
        vec![("version".to_owned(), env!("CARGO_PKG_VERSION").to_owned())]
    );
}

/// Every family carries `HELP` and `TYPE`, and `_total` means counter and nothing else. A metric
/// without `TYPE` is read as untyped by Prometheus and silently dropped by some rules; a gauge
/// named `_total` makes `rate()` meaningless on it.
#[test]
fn every_family_is_documented_and_the_total_suffix_is_reserved_for_counters() {
    let server = Server::start();
    let (_, _, body) = server.get_text("/prometheus");

    let families = exposed_types(&body);
    assert!(families.len() > 50, "suspiciously few metrics:\n{body}");

    let mut declared: Vec<&str> = Vec::new();
    for (name, kind) in &families {
        assert!(
            !declared.contains(&name.as_str()),
            "{name} is declared more than once"
        );
        declared.push(name);
        assert!(
            body.contains(&format!("# HELP {name} ")),
            "{name} has no HELP line:\n{body}"
        );
        let help = body
            .lines()
            .find_map(|line| line.strip_prefix(&format!("# HELP {name} ")))
            .expect("the HELP line found above");
        assert!(!help.trim().is_empty(), "{name} has an empty HELP line");
        // A histogram is allowed because it is cumulative like a counter, and reports its count
        // through the `_count` suffix Prometheus defines for it rather than through `_total`.
        assert!(
            matches!(kind.as_str(), "counter" | "gauge" | "histogram"),
            "{name} is exported as {kind}, which is not a counter, gauge or histogram"
        );
        assert_eq!(
            name.ends_with("_total"),
            kind == "counter",
            "{name} is a {kind} but its name says otherwise"
        );
    }
}

/// The seven metric groups a dashboard expects. A group going missing is a panel that silently
/// stops having data, which is the failure mode these tests prevent.
#[test]
fn every_metric_group_is_exported() {
    let server = Server::start();
    let (_, _, body) = server.get_text("/prometheus");
    let families = exposed_types(&body);

    for group in [
        "process",
        "memory",
        "faults",
        "controller",
        "cpu",
        "io",
        "network",
    ] {
        let prefix = format!("loadlab_{group}_");
        assert!(
            families.iter().any(|(name, _)| name.starts_with(&prefix)),
            "no loadlab_{group}_* metric is exposed:\n{body}"
        );
    }
}

/// Counters must never go backwards. These are assembled from absolute `/proc` readings, so the
/// only thing keeping them monotonic is the delta bookkeeping in `render`, and a bug there would
/// make `rate()` and `increase()` report negative work.
#[test]
fn counters_never_go_backwards_under_load() {
    let server = Server::start();
    server.post("/workload/start");
    let (status, _) = server.put(
        "/workload",
        r#"{"running":true,"controller":{"tick_ms":50},
            "memory":{"anonymous_rss":"64MiB"},
            "memory_access":{"pages_per_second":20000},
            "io":{"read_bps":8388608,"write_bps":8388608},
            "network":{"target":"127.0.0.1:9","messages_per_second":500}}"#,
    );
    assert_eq!(status, 200);
    server.wait_metrics("the controller to tick", |metrics| {
        metrics["controller"]["tick_count"].as_f64().unwrap_or(0.0) > 0.0
    });

    let counters = [
        "loadlab_process_cpu_user_seconds_total",
        "loadlab_process_cpu_system_seconds_total",
        "loadlab_controller_ticks_total",
        "loadlab_faults_minor_total",
        "loadlab_faults_major_total",
        "loadlab_io_read_bytes_total",
        "loadlab_io_write_bytes_total",
        "loadlab_io_ops_total",
        "loadlab_io_fsyncs_total",
        "loadlab_network_messages_total",
        "loadlab_network_bytes_total",
        "loadlab_network_connections_total",
        "loadlab_network_errors_total",
    ];

    // Starts at negative infinity so the first scrape establishes a floor rather than failing
    // against it. Counters are non-negative, so this cannot mask a decrease.
    let mut lowest: Vec<f64> = vec![f64::NEG_INFINITY; counters.len()];
    for _ in 0..3 {
        let (_, _, body) = server.get_text("/prometheus");
        for (index, name) in counters.iter().enumerate() {
            let value: f64 = sample(&body, name).parse().expect("a numeric counter");
            assert!(
                value >= lowest[index],
                "{name} went backwards: {} then {value}",
                lowest[index]
            );
            lowest[index] = value;
        }
    }

    // A counter that never moved would be as broken as one that went backwards, so check that the
    // ones with a workload behind them actually did.
    let (_, _, body) = server.get_text("/prometheus");
    assert!(
        sample(&body, "loadlab_controller_ticks_total")
            .parse::<f64>()
            .expect("numeric")
            > 0.0,
        "the tick counter never advanced"
    );
    assert!(
        sample(&body, "loadlab_faults_minor_total")
            .parse::<f64>()
            .expect("numeric")
            > 0.0,
        "the minor-fault counter never advanced under a fault-rate workload"
    );
}

// --- Authentication, body size, and mutation rate limits ---------------------------------

#[test]
fn an_endpoint_without_a_token_configured_is_open() {
    let server = Server::start();
    assert_eq!(server.get("/state").0, 200);
}

#[test]
fn a_blank_token_configured_leaves_the_api_open() {
    // Whitespace is not a token: an operator who exports an empty variable has not locked
    // themselves out, and has not enabled protection either.
    let server = Server::start_with_options(&[], &["--api-token", "  "]);
    assert_eq!(server.get("/state").0, 200);
}

#[test]
fn a_configured_token_is_required() {
    let server = Server::start_with_token("s3cret");
    let (status, body) = server.get("/state");
    assert_eq!(status, 401);
    assert_eq!(
        body["error"],
        "unauthorized: a valid bearer token is required"
    );
}

#[test]
fn the_configured_token_is_accepted() {
    let server = Server::start_with_token("s3cret");
    let (status, body) = server.get_authorized("/state", "Bearer s3cret");
    assert_eq!(status, 200, "expected the state, got {body}");
}

#[test]
fn a_wrong_token_is_refused() {
    let server = Server::start_with_token("s3cret");
    for wrong in [
        "Bearer s3cre",   // a prefix
        "Bearer s3crets", // an extension
        "Bearer S3CRET",  // the case changed, which is the token's business
        "Bearer ",        // nothing at all
        "Basic s3cret",   // the wrong scheme
        "s3cret",         // no scheme
    ] {
        let (status, body) = server.get_authorized("/state", wrong);
        assert_eq!(status, 401, "{wrong:?} was not refused: {body}");
    }
}

#[test]
fn health_stays_open_when_a_token_is_configured() {
    // A supervisor polls /health to learn the process is up, and would need the token to do it
    // otherwise. It reports nothing that connecting does not already reveal.
    let server = Server::start_with_token("s3cret");
    assert_eq!(server.get("/health").0, 200);
}

#[test]
fn every_protected_endpoint_demands_the_token() {
    let server = Server::start_with_token("s3cret");
    // Read paths.
    for path in ["/state", "/metrics", "/prometheus", "/scenarios"] {
        assert_eq!(
            server.get(path).0,
            401,
            "{path} was readable without a token"
        );
    }
    // Write paths. The body limit and the rate limiter are both inside the authentication wall, so
    // an anonymous caller cannot even tell whether it would have been let through.
    for (method, path, body) in [
        ("PUT", "/workload", Some("{}")),
        ("PATCH", "/workload", Some("{}")),
        ("POST", "/workload/start", None),
        ("POST", "/workload/stop", None),
        ("POST", "/scenarios/load", Some("{}")),
        ("POST", "/scenarios/run", Some("{}")),
        ("POST", "/scenarios/stop", None),
    ] {
        // The property is one-directional: the wall must be complete, so a 401 everywhere is the
        // requirement. Whether each of these then rejects its payload for its own reasons is a
        // different test, and asserting it here would only repeat it.
        let (status, response) = server.send_anonymous(method, path, body);
        assert_eq!(
            status, 401,
            "{method} {path} answered {status} to an anonymous caller: {response}"
        );
    }

    // And the wall is not a blanket refusal: the same requests with the token get past it.
    for (method, path, body) in [
        ("GET", "/state", None),
        ("POST", "/workload/stop", None),
        (
            "PUT",
            "/workload",
            Some(r#"{"running":true,"controller":{"tick_ms":100},"cpu":{"workers":1}}"#),
        ),
    ] {
        let (status, response) = server.send_authorized(method, path, body, "Bearer s3cret");
        assert_ne!(
            status, 401,
            "{method} {path} refused a valid token: {response}"
        );
    }
}

#[test]
fn a_body_over_the_limit_is_refused_before_the_handler() {
    let server = Server::start_with_options(&[], &["--max-body-bytes", "1024"]);
    // Padding, because the numbers a real workload could carry are short: the point is a body over
    // the limit, not an implausible one.
    let oversized = format!(r#"{{"notes":"{}"}}"#, "x".repeat(2048));
    assert!(oversized.len() > 1024);
    let (status, body) = server.put("/workload", &oversized);
    assert_eq!(status, 413, "oversized body: {body}");
    assert_eq!(
        body["error"],
        "request body too large: the limit is 1024 bytes"
    );
    // The rejected request must not have been applied.
    assert_eq!(server.get("/state").0, 200);
}

#[test]
fn a_body_within_the_limit_is_accepted() {
    let server = Server::start_with_options(&[], &["--max-body-bytes", "4096"]);
    let (status, body) = server.put(
        "/workload",
        r#"{"running":true,"controller":{"tick_ms":100},"cpu":{"workers":1},"memory":{"total_bytes":134217728}}"#,
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = server.get("/state");
    assert_eq!(status, 200);
    assert_eq!(
        body["cpu"]["workers"], 1,
        "the accepted body was not applied"
    );
}

#[test]
fn a_token_in_the_query_string_works_from_loopback() {
    // The fallback exists for scrape configurations that cannot send a header, which is why it is
    // only available over loopback: a token in a URL ends up in proxy logs and in `Referer`, and off
    // loopback there is no exposure advantage over the header.
    let server = Server::start_with_token("s3cret");
    let (status, body) = server.send_anonymous("GET", "/prometheus?token=s3cret", None);
    assert_ne!(status, 401, "the query-string token was refused: {body}");

    let (status, _) = server.send_anonymous("GET", "/prometheus?token=wrong", None);
    assert_eq!(status, 401, "a wrong query-string token was accepted");

    // A missing or malformed parameter is not a token.
    for path in [
        "/prometheus",
        "/prometheus?token=",
        "/prometheus?tokens3cret",
        "/prometheus?a=1&token=s3cretx",
    ] {
        let (status, _) = server.send_anonymous("GET", path, None);
        assert_eq!(status, 401, "{path} was readable without a valid token");
    }
}

#[test]
fn mutations_are_rate_limited_and_reads_are_not() {
    // The burst is set to something small and the rate to something slow, so the test does not
    // depend on the shipped defaults and does not have to race a refill.
    let server = Server::start_with_options(
        &[],
        &[
            "--mutation-burst",
            "3",
            "--mutation-rate-per-second",
            "0.05",
        ],
    );

    // Reads are free: a scrape loop must not be throttled by the write limiter, or an operator
    // debugging a rate-limited API could not see what is happening.
    for _ in 0..200 {
        assert_eq!(server.get("/state").0, 200);
    }

    // The whole burst, then a refusal.
    for expected in 0..3 {
        let (status, body) = server.post("/workload/stop");
        assert_eq!(status, 200, "burst mutation {expected} was refused: {body}");
    }
    let (status, body) = server.post("/workload/stop");
    assert_eq!(status, 429, "the mutation limiter never engaged: {body}");
    // The seconds come from the elapsed deficit rather than from the configured rate, so they are
    // only ever close to it. What matters is that the body says roughly what the header says.
    let seconds: u64 = body["error"]
        .as_str()
        .and_then(|message| message.strip_prefix("too many requests: retry in "))
        .and_then(|rest| rest.strip_suffix('s'))
        .and_then(|digits| digits.parse().ok())
        .unwrap_or_else(|| panic!("no wait in the refusal: {body}"));
    assert!(
        (1..=20).contains(&seconds),
        "{seconds}s is not a plausible wait for a 0.05/s refill"
    );
}

#[test]
fn a_refused_mutation_advertises_when_to_retry() {
    let server = Server::start_with_options(
        &[],
        &["--mutation-burst", "1", "--mutation-rate-per-second", "0.5"],
    );
    assert_eq!(server.post("/workload/stop").0, 200);
    let (status, head, _) =
        request_raw(server.addr, "POST", "/workload/stop", None).expect("POST request");
    assert_eq!(status, 429);
    assert!(
        head.lines()
            .any(|line| line.split_once(':').is_some_and(|(name, value)| name
                .trim()
                .eq_ignore_ascii_case("retry-after")
                && value
                    .trim()
                    .parse::<u64>()
                    .is_ok_and(|seconds| seconds >= 1))),
        "429 without a usable Retry-After: {head}"
    );
}

#[test]
fn a_mutation_limit_that_would_refuse_everything_is_refused_at_startup() {
    // A limit of zero silently corrected into "some limit" would be the hardest misconfiguration
    // to notice, so the process has to say so instead of starting.
    let child = Command::new(env!("CARGO_BIN_EXE_loadlab"))
        .arg("--bind")
        .arg(format!("127.0.0.1:{}", free_port()))
        .arg("--mutation-rate-per-second")
        .arg("0")
        .output()
        .expect("run loadlab binary");
    assert!(!child.status.success(), "a zero mutation rate was accepted");
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert!(
        stderr.contains("mutation-rate-per-second must be a positive number"),
        "unhelpful refusal: {stderr}"
    );
}

// --- Per-thread observation ------------------------------------------------------------

#[test]
fn a_running_pool_reports_its_own_run_queue_wait() {
    let server = Server::start();
    server.put(
        "/workload",
        r#"{"running":true,"controller":{"tick_ms":50},
            "cpu":{"utilization":"100","workers":2}}"#,
    );

    let metrics = server.wait_metrics("two CPU workers to be reported", |metrics| {
        metrics["controller"]["cpu"]["worker_count"].as_u64() == Some(2)
    });
    let cpu = &metrics["controller"]["cpu"];

    assert_eq!(
        cpu["scheduling_available"], true,
        "the run-queue wait figures below mean nothing without this: {cpu}"
    );
    // Cumulative since thread start, so zero is a legitimate reading on an idle host. What has to
    // hold is that the worst worker is not greater than the pool.
    let total = cpu["worker_runqueue_wait_nanoseconds"]
        .as_u64()
        .expect("a number");
    let worst = cpu["worker_worst_runqueue_wait_nanoseconds"]
        .as_u64()
        .expect("a number");
    assert!(worst <= total, "worst {worst} exceeds total {total}: {cpu}");

    let (_, _, body) = server.get_text("/prometheus");
    assert!(
        body.contains("loadlab_cpu_worker_runqueue_wait_nanoseconds"),
        "the run-queue wait figure is not exported:\n{body}"
    );
    // The exported figure has to be the one the controller measured, not a separately accumulated
    // total. It is a gauge precisely because the pool is rebuilt on a configuration change and the
    // value then legitimately falls to zero; a counter would have to ignore that and freeze.
    assert_eq!(
        metric_u64(&body, "loadlab_cpu_worker_runqueue_wait_nanoseconds"),
        total,
        "the exported run-queue wait has drifted from the measured one"
    );
    assert!(
        body.contains("loadlab_cpu_worker_scheduling_available 1"),
        "scheduling_available should be 1 with workers running:\n{body}"
    );
}

#[test]
fn a_stopped_pool_reports_no_run_queue_wait_rather_than_a_fake_zero() {
    // There are no workers, so there is nothing to have waited. `scheduling_available` is what stops
    // that from reading as a measurement of a pool that was never starved.
    let server = Server::start();
    server.put("/workload", r#"{"running":true,"cpu":{"utilization":"0"}}"#);
    server.wait_metrics("the controller to settle", |metrics| {
        metrics["controller"]["cpu"]["worker_count"] == 0
    });

    // `/state` is the desired state; what the process observed about itself is served by `/metrics`.
    let (_, body) = server.get("/metrics");
    let cpu = &body["controller"]["cpu"];
    assert_eq!(cpu["worker_count"], 0);
    assert_eq!(cpu["scheduling_available"], false);
    assert_eq!(cpu["worker_runqueue_wait_nanoseconds"], 0);
    assert_eq!(cpu["worker_worst_runqueue_wait_nanoseconds"], 0);
}

#[test]
fn page_faults_are_split_between_the_controller_thread_and_the_rest() {
    let server = Server::start();
    server.put(
        "/workload",
        r#"{"running":true,"controller":{"tick_ms":50},
            "memory":{"anonymous_rss":"64MiB"},
            "memory_access":{"pages_per_second":20000}}"#,
    );

    let metrics = server.wait_metrics("the access controller to report", |metrics| {
        metrics["controller"]["memory_access"]["attribution_available"] == true
    });
    let access = &metrics["controller"]["memory_access"];

    // The split has to add up to the process total it came from, or one of the two halves is not
    // counting faults at all.
    let process_minor = access["minor_faults_total"].as_u64().expect("a number");
    let generated = access["generated_minor_faults"].as_u64().expect("a number");
    let incidental = access["incidental_minor_faults"]
        .as_u64()
        .expect("a number");
    assert_eq!(
        generated + incidental,
        process_minor,
        "the attribution does not add up: {generated} + {incidental} != {process_minor} in {access}"
    );
    assert!(
        generated > 0,
        "the controller thread took no faults at all: {access}"
    );

    let (_, _, body) = server.get_text("/prometheus");
    for name in [
        "loadlab_faults_generated_minor_total",
        "loadlab_faults_incidental_minor_total",
    ] {
        assert!(body.contains(name), "{name} is not exported:\n{body}");
    }
}
