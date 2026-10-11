//! Network traffic: a TCP or UDP generator that targets a host and measures what it actually sent.
//!
//! This is deliberately not an HTTP client. The useful question for a Linux workload laboratory
//! is what the *stack* does under raw load, so the generator speaks TCP and UDP directly and
//! leaves the application protocol out of it. A
//! separate "sink" mode turns loadlab itself into a blackhole, so a workload needs no external
//! destination and the integration tests can run anywhere.
//!
//! The design choices that matter, and why:
//!
//! - **Rates are measured, not modelled.** Every number reported as *actual* is the sum of what the
//!   sockets' `write`/`send` calls returned divided by the tick's wall time. A model would keep
//!   reporting the target through a congested link or a peer that stopped reading, which is exactly
//!   the difference this tool exists to expose.
//! - **A message is one socket write.** `messages_per_second` is the rate of `message_size`-byte
//!   writes over a pool of long-lived TCP connections, or of `message_size`-byte datagrams on UDP.
//!   Fresh TCP connections are a separate rate, `connections_per_second`, because connection churn
//!   exercises a different part of the stack than a bulk transfer does. A fresh connection still
//!   contributes its bytes to `bytes_total`, but not to `messages_total`, so the two targets stay
//!   independent instead of one silently feeding the other.
//! - **The target is chosen by loadlab, never accepted as a path.** It is a `host:port` string the
//!   controller resolves once per change; a workload that does not name a target generates nothing.
//! - **Every socket operation is bounded in time.** A `connect` to a black hole and a write to a
//!   peer that stopped reading are both normal above a generator, so both use a timeout. Otherwise
//!   a single unreachable target would stall the controller thread in a syscall for as long as the
//!   kernel's default (minutes), which a tick loop cannot tolerate.

// The controller is built around message and connection rates, so it converts between `u64` counts
// and `f64` rates throughout. Every conversion is bounded by the per-tick caps below, which are far
// inside `f64`'s exact integer range, so these lints are noise in this module.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::{
    io::{self, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket},
    time::{Duration, Instant},
};

use crate::error::LoadlabError;

/// Upper bound on `concurrency`. One worker thread per slot per tick, so this is a limit on
/// threads, not on requests.
pub const MAX_CONCURRENCY: usize = 256;

/// Upper bound on `message_size`, to keep a single buffer allocation bounded. A UDP datagram cannot
/// exceed this without fragmenting across many IP packets, which is not the workload this models.
pub const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// Hard bound on messages plus connections issued in one tick, independent of the requested rates.
/// Far above any sane target, so it only ever fires as a safety valve against a nonsensical
/// configuration.
const MAX_OPS_PER_TICK: u64 = 1_000_000;

/// Absolute tolerance (messages/connections per second) under which a rate reports convergence.
const RATE_TOLERANCE: f64 = 1.0;

/// Upper bound on each feedback integral, in operations per second.
const MAX_INTEGRAL_OPS: f64 = 1_000_000.0;

/// Proportional gain for the rate feedback loops.
const RATE_KP: f64 = 0.1;

/// How long a single TCP `connect` may take before it is abandoned.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// How long a single `write` or `send` may block before the connection is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// The transport a workload generates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    /// Connection-oriented TCP.
    #[default]
    Tcp,
    /// Connectionless UDP.
    Udp,
}

impl Transport {
    /// The configuration name of this transport.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }

    /// Parse a transport name from a configuration string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "tcp" => Some(Self::Tcp),
            "udp" => Some(Self::Udp),
            _ => None,
        }
    }

    /// Every transport, in documentation order.
    pub const ALL: [Self; 2] = [Self::Tcp, Self::Udp];
}

/// The effective targets for one reconciliation, after the caller has applied the workload's
/// running/stopped state and the operator's ceiling.
#[derive(Debug, Clone)]
pub struct NetworkSettings {
    pub transport: Transport,
    /// The `host:port` to target, or `None` to generate nothing.
    pub target: Option<String>,
    pub concurrency: usize,
    /// Fresh TCP connections per second. Ignored on UDP.
    pub connections_per_second: f64,
    /// Messages per second, each `message_size` bytes.
    pub messages_per_second: f64,
    pub message_size: usize,
}

/// What one reconciliation produced.
#[derive(Debug, Clone, Default)]
pub struct NetworkSnapshot {
    pub transport: String,
    /// The target currently applied, or empty when there is none.
    pub target: String,
    pub concurrency: usize,
    pub message_size: u64,
    pub connections_per_second: f64,
    pub connections_per_second_actual: f64,
    pub messages_per_second: f64,
    pub messages_per_second_actual: f64,
    pub bytes_per_second_actual: f64,
    pub messages_total: u64,
    pub bytes_total: u64,
    pub connections_total: u64,
    pub errors_total: u64,
    pub converged: bool,
    pub saturated: bool,
}

/// A TCP/UDP generator over one resolved target.
pub struct NetworkController {
    transport: Transport,
    /// The target string currently applied, empty when there is none.
    target: String,
    /// The addresses `target` resolved to. Empty when there is no target.
    addrs: Vec<SocketAddr>,
    concurrency: usize,
    message_size: usize,
    /// One persistent connection per worker slot, created on first use and dropped on failure.
    tcp: Vec<Option<TcpStream>>,
    /// One connected datagram socket per worker slot, created on first use.
    udp: Vec<Option<UdpSocket>>,
    /// The operator's process-level ceiling on messages plus connections per second.
    max_ops_per_second: u64,
    last_wall: Option<Instant>,
    last_messages_per_second: f64,
    last_connections_per_second: f64,
    message_integral: f64,
    connection_integral: f64,
    messages_total: u64,
    bytes_total: u64,
    connections_total: u64,
    errors_total: u64,
}

impl NetworkController {
    /// Create a controller bound by the operator's operations-per-second ceiling.
    ///
    /// Nothing is resolved or connected yet: a process that never asks for network traffic never
    /// opens a socket, and the target is resolved when the configuration first names one.
    #[must_use]
    pub fn new(max_ops_per_second: u64) -> Self {
        Self {
            transport: Transport::default(),
            target: String::new(),
            addrs: Vec::new(),
            concurrency: 0,
            message_size: 0,
            tcp: Vec::new(),
            udp: Vec::new(),
            max_ops_per_second,
            last_wall: None,
            last_messages_per_second: 0.0,
            last_connections_per_second: 0.0,
            message_integral: 0.0,
            connection_integral: 0.0,
            messages_total: 0,
            bytes_total: 0,
            connections_total: 0,
            errors_total: 0,
        }
    }

    /// Apply configuration that does not require work, resolving a new target once.
    fn apply_settings(&mut self, settings: &NetworkSettings) -> Result<(), LoadlabError> {
        let requested_target = settings.target.as_deref().unwrap_or("").trim();
        if requested_target != self.target {
            requested_target.clone_into(&mut self.target);
            self.addrs = if self.target.is_empty() {
                Vec::new()
            } else {
                resolve(&self.target)?
            };
            // A new target is a new session: every existing connection and socket points at the old
            // one, so none of them can be reused.
            self.tcp.clear();
            self.udp.clear();
        }

        self.transport = settings.transport;
        self.message_size = settings.message_size.clamp(1, MAX_MESSAGE_SIZE);

        let concurrency = settings.concurrency.clamp(1, MAX_CONCURRENCY);
        if concurrency != self.concurrency {
            self.concurrency = concurrency;
            self.tcp.resize_with(concurrency, || None);
            self.udp.resize_with(concurrency, || None);
        }
        Ok(())
    }

    /// Converge the message and connection targets and report what the controller is doing.
    pub fn reconcile(
        &mut self,
        settings: &NetworkSettings,
    ) -> Result<NetworkSnapshot, LoadlabError> {
        self.apply_settings(settings)?;

        let now = Instant::now();
        let elapsed = self
            .last_wall
            .map_or(0.0, |previous| now.duration_since(previous).as_secs_f64());
        self.last_wall = Some(now);

        // A non-finite target would make every comparison below false and produce a silent
        // controller, so it is treated as zero rather than propagated.
        let messages_per_second = finite_nonnegative(settings.messages_per_second);
        // Connection churn is a TCP-only concept: a UDP "connection" is just a send.
        let connections_per_second = if self.transport == Transport::Tcp {
            finite_nonnegative(settings.connections_per_second)
        } else {
            0.0
        };

        let active = !self.target.is_empty() && elapsed > 0.0;

        let mut saturated = false;
        let mut messages = 0_u64;
        let mut connections = 0_u64;
        if active {
            messages = self.desired(
                messages_per_second,
                self.last_messages_per_second,
                elapsed,
                Integral::Messages,
            );
            connections = self.desired(
                connections_per_second,
                self.last_connections_per_second,
                elapsed,
                Integral::Connections,
            );

            let total = messages.saturating_add(connections);
            let ceiling = capped_ops(finite_nonnegative(self.max_ops_per_second as f64) * elapsed);
            if total > ceiling {
                // Keep the message/connection ratio and drop the excess, so a mixed workload stays
                // mixed when the operator's ceiling binds.
                let ratio = ceiling as f64 / total as f64;
                let scaled_messages = (messages as f64 * ratio).round() as u64;
                messages = scaled_messages.min(ceiling);
                connections = ceiling - messages;
                saturated = true;
            }
        }

        let counts = if messages > 0 || connections > 0 {
            self.execute(messages, connections)
        } else {
            Counts::default()
        };

        let messages_per_second_actual = if active {
            rate(counts.messages, elapsed)
        } else {
            0.0
        };
        let connections_per_second_actual = if active {
            rate(counts.connections, elapsed)
        } else {
            0.0
        };
        let bytes_per_second_actual = if active {
            rate(counts.bytes, elapsed)
        } else {
            0.0
        };

        self.messages_total += counts.messages;
        self.bytes_total += counts.bytes;
        self.connections_total += counts.connections;
        self.errors_total += counts.errors;
        self.last_messages_per_second = messages_per_second_actual;
        self.last_connections_per_second = connections_per_second_actual;

        // Convergence is only claimed against targets that were actually requested. A target of
        // zero means the operator did not ask for that knob, so comparing a measured zero against
        // zero would be vacuous.
        let mut converged = true;
        if messages_per_second > 0.0 {
            converged &= within(messages_per_second_actual, messages_per_second);
        }
        if connections_per_second > 0.0 {
            converged &= within(connections_per_second_actual, connections_per_second);
        }

        Ok(NetworkSnapshot {
            transport: self.transport.as_str().to_owned(),
            target: self.target.clone(),
            concurrency: self.concurrency,
            message_size: self.message_size as u64,
            connections_per_second,
            connections_per_second_actual,
            messages_per_second,
            messages_per_second_actual,
            bytes_per_second_actual,
            messages_total: self.messages_total,
            bytes_total: self.bytes_total,
            connections_total: self.connections_total,
            errors_total: self.errors_total,
            converged,
            saturated,
        })
    }

    /// Operations needed this tick to close the gap to `target`, with feedback.
    ///
    /// The same proportional-plus-integral shape as the other controllers: the proportional term
    /// reacts to this tick's error and the integral absorbs a persistent bias (a link that is
    /// simply slower than asked, for instance) so the loop does not sit at a constant offset.
    fn desired(&mut self, target: f64, measured: f64, elapsed: f64, integral: Integral) -> u64 {
        if target <= 0.0 {
            return 0;
        }
        let integral = match integral {
            Integral::Messages => &mut self.message_integral,
            Integral::Connections => &mut self.connection_integral,
        };
        let error = target - measured;
        *integral = (*integral + error * elapsed).clamp(-MAX_INTEGRAL_OPS, MAX_INTEGRAL_OPS);
        let ops = (target * elapsed + *integral * elapsed + RATE_KP * error * elapsed).max(0.0);
        ops.round().max(0.0) as u64
    }

    /// Fan `messages` and `connections` out across the worker slots.
    ///
    /// Threads are scoped to the call rather than kept in a pool, for the same reason as the I/O
    /// controller: the fan-out is a few per tick and the syscalls dominate the cost. Each worker
    /// owns one persistent connection (or datagram socket) for the life of the tick, so no locking
    /// is needed between them.
    fn execute(&mut self, messages: u64, connections: u64) -> Counts {
        let workers = self.concurrency.clamp(1, MAX_CONCURRENCY);
        let addrs: &[SocketAddr] = &self.addrs;
        let message_size = self.message_size;
        let work = distribute(messages, connections, workers);

        let results: Vec<Counts> = match self.transport {
            Transport::Tcp => {
                let streams = &mut self.tcp;
                std::thread::scope(|scope| {
                    streams
                        .iter_mut()
                        .zip(work.iter())
                        .map(|(stream, work)| {
                            scope.spawn(move || run_tcp_worker(stream, addrs, message_size, work))
                        })
                        .collect::<Vec<_>>()
                        .into_iter()
                        .map(|handle| handle.join().unwrap_or_default())
                        .collect()
                })
            }
            Transport::Udp => {
                let sockets = &mut self.udp;
                std::thread::scope(|scope| {
                    sockets
                        .iter_mut()
                        .zip(work.iter())
                        .map(|(socket, work)| {
                            scope.spawn(move || run_udp_worker(socket, addrs, message_size, work))
                        })
                        .collect::<Vec<_>>()
                        .into_iter()
                        .map(|handle| handle.join().unwrap_or_default())
                        .collect()
                })
            }
        };

        let mut total = Counts::default();
        for counts in results {
            total.messages += counts.messages;
            total.bytes += counts.bytes;
            total.connections += counts.connections;
            total.errors += counts.errors;
        }
        total
    }

    /// Move the clock back so the next [`Self::reconcile`] measures a known window.
    ///
    /// Test-only: the convergence assertions need a deterministic `elapsed`, and a real sleep would
    /// make them slow and flaky.
    #[cfg(test)]
    fn force_elapsed(&mut self, seconds: f64) {
        self.last_wall = Instant::now().checked_sub(Duration::from_secs_f64(seconds));
    }
}

/// Which feedback loop a desired operation count belongs to.
#[derive(Debug, Clone, Copy)]
enum Integral {
    Messages,
    Connections,
}

/// How much work one worker slot should do in a tick.
#[derive(Debug, Clone, Copy, Default)]
struct Work {
    messages: u64,
    connections: u64,
}

/// Spread `messages` and `connections` across `workers` slots as evenly as possible.
fn distribute(messages: u64, connections: u64, workers: usize) -> Vec<Work> {
    let workers64 = workers as u64;
    (0..workers)
        .map(|index| {
            let index = index as u64;
            Work {
                messages: messages / workers64 + u64::from(messages % workers64 > index),
                connections: connections / workers64 + u64::from(connections % workers64 > index),
            }
        })
        .collect()
}

/// Actual operations and bytes completed.
#[derive(Debug, Clone, Copy, Default)]
struct Counts {
    messages: u64,
    bytes: u64,
    connections: u64,
    errors: u64,
}

/// Run one worker's share of the TCP work on its own persistent connection.
///
/// The persistent connection carries `work.messages`; each fresh connection in `work.connections`
/// is a `connect`, one message write, and a close. A failed persistent write drops the connection
/// so the next tick reconnects rather than looping against a dead peer.
fn run_tcp_worker(
    stream: &mut Option<TcpStream>,
    addrs: &[SocketAddr],
    message_size: usize,
    work: &Work,
) -> Counts {
    let mut counts = Counts::default();

    if work.messages > 0 {
        if stream.is_none() {
            match connect_tcp(addrs) {
                Ok(connection) => *stream = Some(connection),
                Err(_) => counts.errors += work.messages,
            }
        }
        if let Some(connection) = stream.as_mut() {
            let buffer = pattern_buffer(message_size);
            for _ in 0..work.messages {
                if connection.write_all(&buffer).is_ok() {
                    counts.messages += 1;
                    counts.bytes += message_size as u64;
                } else {
                    counts.errors += 1;
                    *stream = None;
                    break;
                }
            }
        }
    }

    if work.connections > 0 {
        let buffer = pattern_buffer(message_size);
        for _ in 0..work.connections {
            match connect_tcp(addrs) {
                Ok(mut connection) => {
                    counts.connections += 1;
                    match connection.write_all(&buffer) {
                        Ok(()) => counts.bytes += message_size as u64,
                        Err(_) => counts.errors += 1,
                    }
                }
                Err(_) => counts.errors += 1,
            }
        }
    }

    counts
}

/// Run one worker's share of the UDP work on its own connected datagram socket.
fn run_udp_worker(
    socket: &mut Option<UdpSocket>,
    addrs: &[SocketAddr],
    message_size: usize,
    work: &Work,
) -> Counts {
    let mut counts = Counts::default();
    if work.messages == 0 {
        return counts;
    }

    if socket.is_none() {
        if let Ok(connected) = connect_udp(addrs) {
            *socket = Some(connected);
        } else {
            counts.errors += work.messages;
            return counts;
        }
    }
    let Some(connected) = socket.as_mut() else {
        counts.errors += work.messages;
        return counts;
    };

    let buffer = pattern_buffer(message_size);
    for _ in 0..work.messages {
        if let Ok(bytes) = connected.send(&buffer) {
            counts.messages += 1;
            counts.bytes += bytes as u64;
        } else {
            // A full send buffer is the datagram equivalent of a saturated link: stop asking for
            // more this tick rather than spinning.
            counts.errors += 1;
            break;
        }
    }
    counts
}

/// Resolve a `host:port` target to at least one address, or an error naming it.
fn resolve(target: &str) -> Result<Vec<SocketAddr>, LoadlabError> {
    let addrs: Vec<SocketAddr> = target
        .to_socket_addrs()
        .map_err(|error| {
            LoadlabError::InvalidConfig(format!(
                "network.target {target:?} could not be resolved: {error}"
            ))
        })?
        .collect();
    if addrs.is_empty() {
        return Err(LoadlabError::InvalidConfig(format!(
            "network.target {target:?} resolved to no addresses"
        )));
    }
    Ok(addrs)
}

/// Open a TCP connection to the first address that accepts within [`CONNECT_TIMEOUT`].
fn connect_tcp(addrs: &[SocketAddr]) -> io::Result<TcpStream> {
    let mut last_error = None;
    for addr in addrs {
        match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
            Ok(connection) => {
                let _ = connection.set_nodelay(true);
                let _ = connection.set_write_timeout(Some(WRITE_TIMEOUT));
                return Ok(connection);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no target addresses")))
}

/// Bind and connect a UDP socket to the target's first address.
fn connect_udp(addrs: &[SocketAddr]) -> io::Result<UdpSocket> {
    let Some(addr) = addrs.first() else {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no target addresses",
        ));
    };
    let bind: SocketAddr = if addr.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let socket = UdpSocket::bind(bind)?;
    let _ = socket.set_write_timeout(Some(WRITE_TIMEOUT));
    socket.connect(*addr)?;
    Ok(socket)
}

/// A non-zero buffer of `size` bytes, so a write moves real data rather than a sparse zero range.
fn pattern_buffer(size: usize) -> Vec<u8> {
    let mut buffer = vec![0_u8; size];
    for (index, byte) in buffer.iter_mut().enumerate() {
        *byte = u8::try_from(index % 251)
            .unwrap_or(0)
            .wrapping_mul(31)
            .wrapping_add(7);
    }
    buffer
}

/// A rate over a window, or zero when the window is empty.
fn rate(delta: u64, elapsed: f64) -> f64 {
    if elapsed > 0.0 {
        delta as f64 / elapsed
    } else {
        0.0
    }
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

/// Run a discarding TCP and UDP sink on `addr` until the future is dropped.
///
/// This is loadlab's blackhole: TCP connections are accepted and their bytes read and thrown away,
/// and UDP datagrams are received and thrown away. It is the destination a workload can be pointed
/// at when the point is to generate traffic rather than to drive an application, and it makes the
/// integration tests self-contained. It binds both protocols on the same address.
pub async fn run_sink(addr: SocketAddr) -> Result<(), LoadlabError> {
    use tokio::io::AsyncReadExt;

    let tcp = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| LoadlabError::Network(format!("sink TCP bind on {addr}: {error}")))?;
    let udp = tokio::net::UdpSocket::bind(addr)
        .await
        .map_err(|error| LoadlabError::Network(format!("sink UDP bind on {addr}: {error}")))?;

    let accept = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = tcp.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 64 * 1024];
                // Read until the peer closes or errors. The bytes are discarded: this is a sink,
                // not a protocol endpoint.
                while matches!(stream.read(&mut buffer).await, Ok(read) if read > 0) {}
            });
        }
    });

    let receive = tokio::spawn(async move {
        let mut buffer = vec![0_u8; 64 * 1024];
        while udp.recv(&mut buffer).await.is_ok() {}
    });

    tokio::select! {
        _ = accept => {}
        _ = receive => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // The float comparisons here are drawn from exact integer counts and rates, so `==` is the
    // correct check and an epsilon would only hide a real arithmetic bug.
    #![allow(clippy::float_cmp)]

    use std::{
        io::Read,
        net::{TcpListener, UdpSocket},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use super::{MAX_OPS_PER_TICK, NetworkController, NetworkSettings, Transport};

    fn settings(target: &str, transport: Transport) -> NetworkSettings {
        NetworkSettings {
            transport,
            target: Some(target.to_owned()),
            concurrency: 1,
            connections_per_second: 0.0,
            messages_per_second: 0.0,
            message_size: 1024,
        }
    }

    /// Wait until `value` reaches `at_least`, or the deadline passes.
    fn wait_for(value: &AtomicU64, at_least: u64) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while value.load(Ordering::Relaxed) < at_least && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn transport_names_round_trip() {
        for transport in Transport::ALL {
            assert_eq!(Transport::parse(transport.as_str()), Some(transport));
        }
        assert_eq!(Transport::parse("sctp"), None);
    }

    #[test]
    fn a_controller_with_no_target_generates_nothing() {
        let mut controller = NetworkController::new(MAX_OPS_PER_TICK);
        let mut settings = settings("", Transport::Tcp);
        settings.target = None;
        settings.messages_per_second = 10_000.0;

        controller.force_elapsed(1.0);
        let snapshot = controller.reconcile(&settings).expect("reconcile");
        assert_eq!(snapshot.messages_total, 0);
        assert_eq!(snapshot.bytes_total, 0);
        assert_eq!(snapshot.messages_per_second_actual, 0.0);
        // A rate with nowhere to send it is a misconfiguration, and reporting "not converged" is
        // how it becomes visible instead of a workload that silently does nothing.
        assert!(!snapshot.converged);
    }

    #[test]
    fn a_target_without_a_port_is_rejected() {
        let mut controller = NetworkController::new(MAX_OPS_PER_TICK);
        let settings = settings("localhost", Transport::Tcp);
        let error = controller
            .reconcile(&settings)
            .expect_err("a target without a port must be rejected");
        assert!(error.to_string().contains("network.target"), "{error}");
    }

    #[test]
    fn tcp_messages_reach_a_local_listener_and_are_measured() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let addr = listener.local_addr().expect("addr");
        let received = Arc::new(AtomicU64::new(0));
        let reader = Arc::clone(&received);
        let handle = thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer = [0_u8; 4096];
            while let Ok(read) = stream.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                reader.fetch_add(read as u64, Ordering::Relaxed);
            }
        });

        let mut controller = NetworkController::new(MAX_OPS_PER_TICK);
        let mut settings = settings(&addr.to_string(), Transport::Tcp);
        settings.messages_per_second = 1000.0;

        let _ = controller.reconcile(&settings).expect("first tick");
        controller.force_elapsed(1.0);
        let snapshot = controller.reconcile(&settings).expect("second tick");

        wait_for(&received, 1024);
        assert!(
            received.load(Ordering::Relaxed) >= 1024,
            "the listener should have received bytes"
        );
        assert!(snapshot.messages_total > 0, "messages should be counted");
        assert!(snapshot.bytes_total >= 1024);
        assert!(snapshot.bytes_per_second_actual > 0.0);

        // Dropping the controller closes the persistent connection, ending the reader.
        drop(controller);
        handle.join().expect("reader");
    }

    #[test]
    fn tcp_connections_are_opened_and_counted() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let addr = listener.local_addr().expect("addr");
        listener.set_nonblocking(true).expect("nonblocking");
        let accepted = Arc::new(AtomicU64::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let counter = Arc::clone(&accepted);
        let flag = Arc::clone(&running);
        let handle = thread::spawn(move || {
            while flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((_stream, _)) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        let mut controller = NetworkController::new(MAX_OPS_PER_TICK);
        let mut settings = settings(&addr.to_string(), Transport::Tcp);
        settings.connections_per_second = 100.0;
        settings.message_size = 64;

        let _ = controller.reconcile(&settings).expect("first tick");
        controller.force_elapsed(1.0);
        let snapshot = controller.reconcile(&settings).expect("second tick");

        wait_for(&accepted, 1);
        assert!(
            accepted.load(Ordering::Relaxed) > 0,
            "the listener should have accepted connections"
        );
        assert!(snapshot.connections_total > 0);
        assert!(snapshot.connections_per_second_actual > 0.0);

        running.store(false, Ordering::Relaxed);
        handle.join().expect("acceptor");
    }

    #[test]
    fn udp_datagrams_reach_a_local_socket_and_are_measured() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("socket");
        let addr = socket.local_addr().expect("addr");
        let received = Arc::new(AtomicU64::new(0));
        let reader = Arc::clone(&received);
        let handle = thread::spawn(move || {
            let mut buffer = [0_u8; 2048];
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
                match socket.recv_from(&mut buffer) {
                    Ok((read, _)) => {
                        reader.fetch_add(read as u64, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            }
        });

        let mut controller = NetworkController::new(MAX_OPS_PER_TICK);
        let mut settings = settings(&addr.to_string(), Transport::Udp);
        settings.messages_per_second = 100.0;
        settings.message_size = 512;

        let _ = controller.reconcile(&settings).expect("first tick");
        controller.force_elapsed(1.0);
        let snapshot = controller.reconcile(&settings).expect("second tick");

        wait_for(&received, 512);
        assert!(
            received.load(Ordering::Relaxed) >= 512,
            "the socket should have received datagrams"
        );
        assert!(snapshot.messages_total > 0);
        assert!(snapshot.bytes_total >= 512);

        handle.join().expect("receiver");
    }

    #[test]
    fn the_operator_ceiling_saturates_a_large_request() {
        // Port 1 on loopback is not listening, so no traffic is generated; what is asserted is the
        // ceiling itself.
        let mut controller = NetworkController::new(10);
        let mut settings = settings("127.0.0.1:1", Transport::Tcp);
        settings.messages_per_second = 1_000_000.0;

        let _ = controller.reconcile(&settings).expect("first tick");
        controller.force_elapsed(1.0);
        let snapshot = controller.reconcile(&settings).expect("second tick");
        assert!(
            snapshot.saturated,
            "the ceiling should saturate the request"
        );
    }
}
