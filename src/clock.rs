//! The clock expressions are evaluated against.
//!
//! Expressions are written in terms of `t`, in seconds, and `t = 0` is the moment the workload
//! starts. The clock re-anchors whenever the workload transitions from stopped to running, so a
//! restart begins again from zero.
//!
//! It is deliberately a property of the run rather than of the process: process uptime keeps
//! counting while the workload is paused, and a `t` that carried on across a restart would make the
//! same stated workload mean something different the second time it ran.

use std::time::Instant;

/// The origin of `t` for the current run.
///
/// [`Self::observe`] is called once per tick with whether the workload is running, and the origin
/// moves only when a run actually begins. Everything else — a tick that takes longer, a controller
/// that errors, a client polling `/metrics` — leaves `t` alone, because a clock that jumped around
/// would make the expression targets jump with it.
#[derive(Debug)]
pub struct RunClock {
    /// The instant `t` is measured from.
    origin: Instant,
    /// Whether the workload was running at the last observation.
    running: bool,
}

impl RunClock {
    /// A clock anchored to now, as if no run had started yet.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            running: false,
        }
    }

    /// Re-anchor if a run just started.
    ///
    /// The origin moves on the stopped-to-running transition and nowhere else. After that, `t` is
    /// served by the monotonic clock, which does not go backwards when the wall clock is stepped.
    pub fn observe(&mut self, running: bool) {
        if running && !self.running {
            self.origin = Instant::now();
        }
        self.running = running;
    }

    /// Seconds since `t = 0`, which is what expressions are evaluated against.
    pub fn elapsed_seconds(&self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }
}

impl Default for RunClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::thread::sleep;
    use std::time::Duration;

    use super::RunClock;

    #[test]
    fn t_is_zero_when_the_run_starts() {
        let mut clock = RunClock::new();
        // A clock that has never seen a running workload reports near-zero time since origin.
        assert!(clock.elapsed_seconds() < 0.1);
        clock.observe(true);
        assert!(
            clock.elapsed_seconds() < 0.1,
            "t should restart at zero when the run starts"
        );
    }

    #[test]
    fn t_advances_while_running_and_restarts_on_a_new_run() {
        let mut clock = RunClock::new();
        clock.observe(true);
        sleep(Duration::from_millis(20));
        assert!(clock.elapsed_seconds() > 0.01, "t did not advance");

        // Stopping and starting again anchors to the new start rather than the old one.
        clock.observe(false);
        clock.observe(true);
        assert!(
            clock.elapsed_seconds() < 0.01,
            "t should restart when a new run begins"
        );
    }

    #[test]
    fn a_tick_that_changes_nothing_leaves_t_alone() {
        let mut clock = RunClock::new();
        clock.observe(true);
        sleep(Duration::from_millis(20));
        let before = clock.elapsed_seconds();
        // Still running: the origin must not move.
        clock.observe(true);
        assert!(clock.elapsed_seconds() >= before);
    }
}
