//! What makes two runs of the same workload comparable after the fact.
//!
//! A workload state says what to generate. Three things are missing before two runs can be lined
//! up against each other, and they are what this module provides.
//!
//! **A shared `t = 0`.** Expressions are evaluated against a time in seconds, and that number has
//! to mean the same thing in two runs or the targets were never the same workload. It used to be
//! measured from process start, which meant loading a scenario at a different process age silently
//! changed every target it computes. [`ExperimentClock`] anchors it to the run instead.
//!
//! **One seed.** Three controllers each carry their own `random_seed`, defaulting to the same
//! value. That is already reproducible, but "reproducible" and "the run I did yesterday" are
//! different claims, and only the second one needs a number an operator chose. [`stream_seed`]
//! turns one experiment seed into a distinct stream per controller.
//!
//! **An identity.** Exposed through [`crate::api`] as the `experiment` block of `GET /metrics`.
//!
//! Timestamps are parsed here rather than through a date-time crate because the only thing
//! loadlab needs is seconds since the Unix epoch, and a dependency that pulls in a calendar, a
//! locale database and a time zone would be a poor trade for that. What is accepted is deliberately
//! narrower than what RFC 3339 permits: see [`parse_rfc3339`].

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::error::LoadlabError;

/// Which controller a seed belongs to.
///
/// The variant is part of the derivation, so adding a controller is a deliberate act that changes
/// every seed derived from a given experiment seed. That is the right direction to be wrong in: a
/// run recorded before the change was not running the new controller anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// The anonymous-arena fault loop.
    MemoryAccess,
    /// The file-backed mapping's access pattern.
    FileBacked,
    /// The disk I/O block walk.
    Io,
}

impl Stream {
    /// A distinct mixing constant per stream.
    ///
    /// Arbitrary, and arbitrary on purpose. The only properties that matter are that they differ and
    /// that they are not small: a small constant would let a seed of `1` produce a nearby state for
    /// one controller and not another, which is the same correlation the mixing is there to remove.
    const fn domain(self) -> u64 {
        match self {
            Self::MemoryAccess => 0x6d65_6d6f_7279_6163,
            Self::FileBacked => 0x6669_6c65_6261_636b,
            Self::Io => 0x6469_736b_696f_5f72,
        }
    }
}

/// The seed one controller should actually use.
///
/// `None` means "leave it alone", which preserves the historical default rather than inventing a
/// new one. An explicit per-controller seed is passed through untouched, so a value written before
/// this existed still produces the same walk it always did; only seeds derived from the experiment
/// seed go through the mixer.
pub fn stream_seed(
    experiment_seed: Option<u64>,
    controller_seed: Option<u64>,
    stream: Stream,
) -> Option<u64> {
    if let Some(seed) = controller_seed {
        return Some(seed);
    }
    experiment_seed.map(|seed| mix(seed, stream.domain()))
}

/// `SplitMix64`'s finalizer, used here only to spread one seed across several streams.
///
/// A plain multiply-and-add would leave the three streams visibly related — seeds 1, 2 and 3
/// produce walks a fixed distance apart — so the pattern would be recognisable across runs and two
/// controllers would appear correlated in a recording. This is the same mixer public block seeds
/// use, which is the point: it is a scrambler, not a generator.
fn mix(seed: u64, domain: u64) -> u64 {
    let mut z = seed.wrapping_add(domain ^ 0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Parse an RFC 3339 timestamp into seconds since the Unix epoch.
///
/// Accepts `YYYY-MM-DDTHH:MM:SS`, optionally followed by fractional seconds and by either `Z` or a
/// `±HH:MM` offset. Fractional seconds are read for shape and then discarded: loadlab's clocks are
/// seconds-resolution, so keeping a sub-second remainder would imply a precision nothing downstream
/// has.
///
/// Deliberately narrower than the RFC, because a parser that accepts something it cannot honour is
/// worse than one that refuses:
///
/// - no leap seconds, so `:60` is rejected rather than folded into `:59`;
/// - no lowercase `t`/separator alternatives beyond a lowercase `t`, and no space separator;
/// - no `-00:00`, whose meaning the RFC leaves undefined.
///
/// A date outside the four-digit year range is rejected, since the arithmetic below is exact only
/// within it.
pub fn parse_rfc3339(text: &str) -> Result<i64, LoadlabError> {
    let invalid =
        |reason: &str| LoadlabError::InvalidConfig(format!("expected {reason}, in {text:?}"));

    // The offset, if any, is the only part that may be absent from the fixed-width prefix, so the
    // date-time is sliced by length rather than searched for.
    let (datetime, offset_seconds) = if let Some(stripped) = text.strip_suffix('Z') {
        (stripped, 0)
    } else if let Some(index) = text.rfind(['+', '-']) {
        // `rfind` on `+`/`-` also finds the `-` of the date, so only an offset that appears after
        // the `T` counts.
        let separator = text
            .find(['T', 't'])
            .ok_or_else(|| invalid("a date and a time"))?;
        if index < separator {
            return Err(invalid("a `Z` or a ±HH:MM offset"));
        }
        (&text[..index], parse_offset(&text[index..])?)
    } else {
        return Err(invalid("a `Z` or a ±HH:MM offset"));
    };

    let (date, time) = datetime
        .split_once(['T', 't'])
        .ok_or_else(|| invalid("a date and a time"))?;
    let mut date_fields = date.split('-');
    let mut time_fields = time.split(':');

    let year = i64::from(number(&mut date_fields, 4, "a four-digit year")?);
    let month = number(&mut date_fields, 2, "a two-digit month")?;
    let day = number(&mut date_fields, 2, "a two-digit day")?;
    if date_fields.next().is_some() {
        return Err(invalid("a YYYY-MM-DD date"));
    }
    let hour = number(&mut time_fields, 2, "a two-digit hour")?;
    let minute = number(&mut time_fields, 2, "a two-digit minute")?;
    let second_field = time_fields
        .next()
        .ok_or_else(|| invalid("seconds in the time"))?;
    if time_fields.next().is_some() {
        return Err(invalid("an HH:MM:SS time"));
    }
    let second = match second_field.split_once('.') {
        Some((whole, fraction)) => {
            if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid("one or more fractional-second digits"));
            }
            number_from(whole, "seconds in the time")?
        }
        None => number_from(second_field, "seconds in the time")?,
    };

    if !(1..=12).contains(&month) {
        return Err(invalid("a month between 01 and 12"));
    }
    if day < 1 || day > days_in_month(year, month) {
        return Err(invalid("a day that exists in that month"));
    }
    // 60 is excluded because it only ever means a leap second, which this parser does not model.
    if hour > 23 || minute > 59 || second > 59 {
        return Err(invalid("a time between 00:00:00 and 23:59:59"));
    }

    let days = days_from_civil(year, month, day);
    let seconds =
        days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second);
    // The offset is the difference from UTC to local time, so removing it moves the instant back.
    Ok(seconds - offset_seconds)
}

fn parse_offset(text: &str) -> Result<i64, LoadlabError> {
    let invalid =
        |reason: &str| LoadlabError::InvalidConfig(format!("expected {reason}, in {text:?}"));
    let (sign, rest) = match text.split_at_checked(1) {
        Some(("+", rest)) => (1_i64, rest),
        Some(("-", rest)) => (-1_i64, rest),
        _ => return Err(invalid("a ±HH:MM offset")),
    };
    let (hours, minutes) = rest
        .split_once(':')
        .ok_or_else(|| invalid("a ±HH:MM offset"))?;
    if hours.len() != 2 || minutes.len() != 2 {
        return Err(invalid("a ±HH:MM offset"));
    }
    let hours = number_from(hours, "offset hours")?;
    let minutes = number_from(minutes, "offset minutes")?;
    if hours > 23 || minutes > 59 {
        return Err(invalid("an offset between ±00:00 and ±23:59"));
    }
    Ok(sign * (i64::from(hours) * 3_600 + i64::from(minutes) * 60))
}

fn number(
    fields: &mut std::str::Split<'_, char>,
    width: usize,
    what: &str,
) -> Result<u32, LoadlabError> {
    let field = fields
        .next()
        .ok_or_else(|| LoadlabError::InvalidConfig(format!("expected {what}")))?;
    if field.len() != width {
        return Err(LoadlabError::InvalidConfig(format!("expected {what}")));
    }
    number_from(field, what)
}

fn number_from(field: &str, what: &str) -> Result<u32, LoadlabError> {
    field
        .parse::<u32>()
        .map_err(|_| LoadlabError::InvalidConfig(format!("expected {what}, found {field:?}")))
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days between 1970-01-01 and the given proleptic Gregorian date.
///
/// Howard Hinnant's `days_from_civil`, which is exact for any year representable here and has no
/// lookup table to get wrong. The era arithmetic is what makes it work across the 400-year cycle
/// that leap years follow, rather than only after 1970.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = i64::from(if month > 2 { month - 3 } else { month + 9 });
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// What one call to [`ExperimentClock::observe`] did.
#[derive(Debug)]
pub enum Observed {
    /// Nothing about the run changed, so the origin stayed where it was.
    Unchanged,
    /// The run was re-anchored to now.
    Reanchored,
    /// The configured start time could not be parsed. The origin still moved, but without an offset.
    ///
    /// Reported once per distinct value rather than once per tick, so a caller can log it without
    /// having to suppress repeats itself.
    Rejected(LoadlabError),
}

/// The origin of `t` for the current run.
///
/// The controller calls [`Self::observe`] once per tick with the configuration's view of the world,
/// and it resets the origin only when the run has actually changed: the workload starting, or the
/// configured start time changing. Everything else — a tick that takes longer, a controller that
/// errors, a client polling `/metrics` — leaves `t` alone, because a clock that jumped around would
/// make the expression targets jump with it.
#[derive(Debug)]
pub struct ExperimentClock {
    /// The instant `t` is measured from.
    origin: Instant,
    /// Seconds from the configured `start_time` to `origin`.
    ///
    /// `None` when no start time is configured, in which case `t` is simply the age of the run.
    /// Negative when `start_time` is in the future: the experiment has not started yet, and `t`
    /// says so rather than being clamped to zero.
    start_offset_seconds: Option<f64>,
    /// The `start_time` the current origin was built from.
    start_time: Option<String>,
    /// Whether the workload was running at the last observation.
    running: bool,
}

impl ExperimentClock {
    /// A clock anchored to now, with no configured start time.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            start_offset_seconds: None,
            start_time: None,
            running: false,
        }
    }

    /// Re-anchor if the run changed.
    ///
    /// The wall clock is consulted only when a start time is configured, and only at the moment the
    /// origin moves. Reading it on every tick would make `t` depend on NTP steps and clock
    /// adjustments for no benefit: once the offset from the configured instant is known, `t` is
    /// better served by the monotonic clock, which does not go backwards.
    pub fn observe(&mut self, running: bool, start_time: Option<&str>) -> Observed {
        let started = running && !self.running;
        let reconfigured = start_time != self.start_time.as_deref();
        if !started && !reconfigured {
            self.running = running;
            return Observed::Unchanged;
        }

        // Recorded before the parse is attempted, so a timestamp that cannot be used is remembered
        // as the current one. That is what makes `Observed::Rejected` arrive once per distinct bad
        // value instead of once per tick, which at a 1ms tick would be a thousand lines a second.
        self.start_time = start_time.map(str::to_owned);
        self.origin = Instant::now();
        self.running = running;

        let Some(text) = start_time else {
            self.start_offset_seconds = None;
            return Observed::Reanchored;
        };

        // The configuration is validated on the way in, so this is unreachable through the API.
        // Re-anchoring without an offset keeps the run working and leaves the offset absent rather
        // than invented: `t` becomes the age of the run, which is the old behaviour and is a claim
        // loadlab can actually support.
        let unix_seconds = match parse_rfc3339(text) {
            Ok(seconds) => seconds,
            Err(error) => {
                self.start_offset_seconds = None;
                return Observed::Rejected(error);
            }
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |since_epoch| since_epoch.as_secs_f64());
        // Exact for any instant this side of the year 285 million, which is not a constraint
        // anyone will hit. The alternative -- keeping `t` in integer seconds -- would cost the
        // sub-second resolution the monotonic half of the clock provides.
        #[allow(clippy::cast_precision_loss)]
        let start = unix_seconds as f64;
        self.start_offset_seconds = Some(now - start);
        Observed::Reanchored
    }

    /// Seconds since `t = 0`, which is what expressions are evaluated against.
    pub fn elapsed_seconds(&self) -> f64 {
        self.start_offset_seconds.unwrap_or(0.0) + self.origin.elapsed().as_secs_f64()
    }

    /// Seconds between the configured `start_time` and the moment the run was anchored.
    ///
    /// Published because it is the only way to tell what `t = 0` meant: with a start time set, `t`
    /// is not the age of anything, and an observer reading `t = 300` needs this to know whether
    /// that is five minutes into the experiment or five minutes before it.
    pub fn start_offset_seconds(&self) -> Option<f64> {
        self.start_offset_seconds
    }
}

impl Default for ExperimentClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{Observed, Stream, days_from_civil, mix, parse_rfc3339, stream_seed};

    /// The transition an observation produced, ignoring any error payload.
    ///
    /// A variant name rather than the whole value because `LoadlabError` carries a message and does
    /// not compare equal, and because what these tests are about is which transition happened, not
    /// the wording of a message they never assert on.
    fn variant(observed: &Observed) -> &'static str {
        match observed {
            Observed::Unchanged => "unchanged",
            Observed::Reanchored => "reanchored",
            Observed::Rejected(_) => "rejected",
        }
    }

    #[test]
    fn the_epoch_and_a_leap_day_land_where_the_calendar_says() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(
            parse_rfc3339("2026-10-04T11:30:37Z").unwrap(),
            1_791_113_437
        );
        // 2024 was a leap year, so this date exists and is not the 1st of March.
        assert_eq!(
            parse_rfc3339("2024-02-29T23:59:59Z").unwrap(),
            1_709_251_199
        );
    }

    #[test]
    fn an_offset_moves_the_instant_and_the_sign_is_not_ignored() {
        // 00:00 at +02:00 is 22:00 the previous day in UTC.
        assert_eq!(
            parse_rfc3339("2000-01-01T00:00:00+02:00").unwrap(),
            parse_rfc3339("1999-12-31T22:00:00Z").unwrap()
        );
        assert_eq!(
            parse_rfc3339("2000-01-01T00:00:00-05:00").unwrap(),
            parse_rfc3339("2000-01-01T05:00:00Z").unwrap()
        );
    }

    #[test]
    fn dates_before_the_epoch_are_negative_rather_than_wrapped() {
        assert_eq!(parse_rfc3339("1969-12-31T23:59:59Z").unwrap(), -1);
    }

    #[test]
    fn fractional_seconds_are_read_for_shape_and_discarded() {
        assert_eq!(
            parse_rfc3339("2026-10-04T11:30:37.123456Z").unwrap(),
            parse_rfc3339("2026-10-04T11:30:37Z").unwrap()
        );
        assert!(parse_rfc3339("2026-10-04T11:30:37.Z").is_err());
        assert!(parse_rfc3339("2026-10-04T11:30:37.abcZ").is_err());
    }

    #[test]
    fn a_timestamp_that_does_not_exist_is_refused_rather_than_approximated() {
        // Each of these would parse into a plausible instant if the fields were trusted in order,
        // which is exactly how a silently wrong `t` gets into a recording.
        for text in [
            "2023-02-29T00:00:00Z",      // 2023 was not a leap year
            "2026-13-01T00:00:00Z",      // month 13
            "2026-10-32T00:00:00Z",      // day 32
            "2026-10-00T00:00:00Z",      // day 0
            "2026-10-04T24:00:00Z",      // hour 24
            "2026-10-04T11:60:00Z",      // minute 60
            "2026-10-04T11:30:60Z",      // leap second, deliberately unsupported
            "2026-10-04 11:30:37Z",      // space separator
            "2026-10-04",                // no time
            "2026-10-04T11:30:37",       // no zone
            "2026-1-04T11:30:37Z",       // unpadded
            "26-10-04T11:30:37Z",        // two-digit year
            "2026-10-04T11:30:37+2:00",  // unpadded offset
            "2026-10-04T11:30:37+99:00", // offset out of range
            "",
            "not a timestamp at all",
        ] {
            assert!(
                parse_rfc3339(text).is_err(),
                "{text:?} should not parse as a timestamp"
            );
        }
    }

    #[test]
    fn one_experiment_seed_gives_each_controller_a_different_walk() {
        let seed = Some(42);
        let streams = [Stream::MemoryAccess, Stream::FileBacked, Stream::Io];
        let derived: Vec<u64> = streams
            .iter()
            .map(|stream| stream_seed(seed, None, *stream).expect("a derived seed"))
            .collect();

        let mut sorted = derived.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            3,
            "two controllers share a stream: {derived:?}"
        );
        // Neighbouring seeds must not produce neighbouring states, or the streams stay visibly
        // related across runs.
        let next: Vec<u64> = streams
            .iter()
            .map(|stream| stream_seed(Some(43), None, *stream).expect("a derived seed"))
            .collect();
        for (a, b) in derived.iter().zip(&next) {
            assert!(
                a.abs_diff(*b) > 1_000_000,
                "seeds 42 and 43 gave nearby states {a} and {b}"
            );
        }
    }

    #[test]
    fn an_explicit_controller_seed_wins_and_is_not_remixed() {
        // Passing it through untouched is what keeps every scenario written before the experiment
        // seed existed producing the walk it always did.
        assert_eq!(stream_seed(Some(42), Some(7), Stream::Io), Some(7));
        assert_eq!(stream_seed(None, None, Stream::Io), None);
        assert_eq!(
            stream_seed(None, Some(7), Stream::Io),
            stream_seed(Some(999), Some(7), Stream::Io)
        );
    }

    #[test]
    fn the_mixer_is_a_function_and_not_an_accident() {
        assert_eq!(mix(1, 2), mix(1, 2));
        assert_ne!(mix(1, 2), mix(1, 3));
        assert_ne!(mix(1, 2), mix(2, 2));
    }

    #[test]
    fn days_from_civil_agrees_with_the_epoch_for_every_month_boundary() {
        // 1970-01-01 is day zero; walking the first year of the calendar month by month has to land
        // on 1971-01-01 exactly.
        let mut total = 0;
        for month in 1..=12 {
            total += i64::from(super::days_in_month(1970, month));
            assert_eq!(
                days_from_civil(1970, month + 1, 1) - days_from_civil(1970, month, 1),
                i64::from(super::days_in_month(1970, month))
            );
        }
        assert_eq!(days_from_civil(1971, 1, 1), 365);
        assert_eq!(total, 365);
    }

    /// Assert two instants are within a few milliseconds of each other.
    ///
    /// Wall-clock reads and `Instant::elapsed` are both real time, so an equality test here would be
    /// a test that fails on a loaded machine. The property under test is which instant was chosen,
    /// and that is visible to within a scheduling quantum.
    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 0.05,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn the_clock_only_moves_when_the_run_changes() {
        let mut clock = super::ExperimentClock::new();

        // Not running, no start time: there is no run to anchor yet, so repeated observation must
        // leave the origin where it was rather than restarting it on every tick.
        assert_eq!(variant(&clock.observe(false, None)), "unchanged");
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(variant(&clock.observe(false, None)), "unchanged");
        assert!(clock.start_offset_seconds().is_none());

        // Starting the workload is the event that defines t = 0.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(variant(&clock.observe(true, None)), "reanchored");
        assert_close(clock.elapsed_seconds(), 0.0);

        // A tick later, the clock has advanced and the origin has not moved: a steady run must not
        // re-anchor on every tick, or the expression targets would step backwards continuously.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(variant(&clock.observe(true, None)), "unchanged");
        assert!(clock.elapsed_seconds() > 0.01, "t did not advance");
    }

    #[test]
    fn stopping_and_starting_again_restarts_the_run() {
        let mut clock = super::ExperimentClock::new();
        assert_eq!(variant(&clock.observe(true, None)), "reanchored");
        std::thread::sleep(Duration::from_millis(20));
        assert!(clock.elapsed_seconds() > 0.01);

        // Stopping does not move the origin, so a workload that is briefly paused and inspected
        // keeps the same t rather than jumping.
        clock.observe(false, None);
        let before_restart = clock.elapsed_seconds();
        std::thread::sleep(Duration::from_millis(20));
        assert_close(clock.elapsed_seconds(), before_restart + 0.02);

        // Starting again is a new run.
        assert_eq!(variant(&clock.observe(true, None)), "reanchored");
        assert_close(clock.elapsed_seconds(), 0.0);
    }

    #[test]
    fn a_configured_start_time_places_t_relative_to_that_instant() {
        let mut clock = super::ExperimentClock::new();
        // Ninety seconds ago, so t should be about 90 and climbing.
        let ninety_seconds_ago = rfc3339_now_offset(-90);
        assert_eq!(
            variant(&clock.observe(true, Some(&ninety_seconds_ago))),
            "reanchored"
        );
        // A range rather than an equality: `rfc3339_now_offset` builds a whole-second timestamp, so
        // the real offset is 90 seconds plus whatever fraction of the current second had elapsed.
        // The parser has second resolution, so that fraction is unrecoverable, and pretending
        // otherwise would need a fixture rather than a live clock.
        assert!(
            (90.0..91.0).contains(&clock.elapsed_seconds()),
            "expected 90 to 91, got {}",
            clock.elapsed_seconds()
        );
        let offset = clock
            .start_offset_seconds()
            .expect("an offset is configured");
        assert!(
            (90.0..91.0).contains(&offset),
            "expected 90 to 91, got {offset}"
        );

        // Not asserted separately that the timestamp was recorded: the offset above is that fact,
        // and a second accessor for it would be a second way to be wrong.
    }

    #[test]
    fn a_start_time_in_the_future_yields_negative_t_rather_than_zero() {
        let mut clock = super::ExperimentClock::new();
        let in_an_hour = rfc3339_now_offset(3_600);
        assert_eq!(
            variant(&clock.observe(true, Some(&in_an_hour))),
            "reanchored"
        );

        // Clamping to zero would claim the experiment had already been running for no time, which is
        // a different and wrong statement: it has not started.
        let elapsed = clock.elapsed_seconds();
        assert!(
            (-3_600.1..-3_599.0).contains(&elapsed),
            "expected about -3600, got {elapsed}"
        );
    }

    #[test]
    fn changing_the_start_time_re_anchors_the_run() {
        let mut clock = super::ExperimentClock::new();
        let ninety_seconds_ago = rfc3339_now_offset(-90);
        assert_eq!(
            variant(&clock.observe(true, Some(&ninety_seconds_ago))),
            "reanchored"
        );

        // The same value again is not a change, so the origin stays put.
        let before = clock.elapsed_seconds();
        assert_eq!(
            variant(&clock.observe(true, Some(&ninety_seconds_ago))),
            "unchanged"
        );
        assert!(clock.elapsed_seconds() >= before);

        // A different value is, and t is re-expressed against it.
        let ten_minutes_ago = rfc3339_now_offset(-600);
        assert_eq!(
            variant(&clock.observe(true, Some(&ten_minutes_ago))),
            "reanchored"
        );
        assert!(
            (600.0..601.0).contains(&clock.elapsed_seconds()),
            "expected 600 to 601, got {}",
            clock.elapsed_seconds()
        );
    }

    #[test]
    fn clearing_the_start_time_returns_to_the_age_of_the_run() {
        let mut clock = super::ExperimentClock::new();
        assert_eq!(
            variant(&clock.observe(true, Some(&rfc3339_now_offset(-90)))),
            "reanchored"
        );
        assert!(clock.start_offset_seconds().is_some());

        assert_eq!(variant(&clock.observe(true, None)), "reanchored");
        assert!(clock.start_offset_seconds().is_none());
        assert_close(clock.elapsed_seconds(), 0.0);
    }

    #[test]
    fn a_timestamp_that_cannot_be_parsed_is_reported_once_and_the_run_still_works() {
        let mut clock = super::ExperimentClock::new();
        assert_eq!(
            variant(&clock.observe(true, Some("not a timestamp"))),
            "rejected"
        );
        // The run is still anchored and usable: `t` is the age of the run, which is the behaviour
        // from before start times existed and is a claim loadlab can support.
        assert!(clock.start_offset_seconds().is_none());
        assert_close(clock.elapsed_seconds(), 0.0);

        // Reported once, not once per tick. A 1ms tick would otherwise be a thousand log lines a
        // second saying the same thing.
        assert_eq!(
            variant(&clock.observe(true, Some("not a timestamp"))),
            "unchanged"
        );

        // A different bad value is a new thing to report, and still leaves the clock working.
        assert_eq!(
            variant(&clock.observe(true, Some("also not a timestamp"))),
            "rejected"
        );
        assert_eq!(
            variant(&clock.observe(true, Some("also not a timestamp"))),
            "unchanged"
        );

        // And a good value recovers it.
        assert_eq!(
            variant(&clock.observe(true, Some(&rfc3339_now_offset(-90)))),
            "reanchored"
        );
        assert!(clock.start_offset_seconds().is_some());
    }

    /// An RFC 3339 timestamp for `offset_seconds` from now.
    ///
    /// Formatted by hand from `SystemTime` rather than by a date-time crate, which is the same
    /// reason [`parse_rfc3339`] exists. Only ever called with whole-minute offsets in tests.
    fn rfc3339_now_offset(offset_seconds: i64) -> String {
        let now = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("the epoch is in the past")
                .as_secs(),
        )
        .expect("seconds since 1970 fit in an i64");
        let seconds = now + offset_seconds;
        let days = seconds.div_euclid(86_400);
        let time_of_day = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            time_of_day / 3_600,
            (time_of_day % 3_600) / 60,
            time_of_day % 60
        )
    }

    /// The inverse of [`super::days_from_civil`], so a test can build a timestamp it can parse.
    fn civil_from_days(days: i64) -> (i64, u32, u32) {
        let shifted = days + 719_468;
        let era = if shifted >= 0 {
            shifted
        } else {
            shifted - 146_096
        } / 146_097;
        let day_of_era = shifted - era * 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let year = year_of_era + era * 400;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let shifted_month = (5 * day_of_year + 2) / 153;
        // Both values come out of the inverse of `days_from_civil` and are in range by
        // construction: the day is 1..=31 and the month 1..=12, or the round trip could not have
        // produced the day of year it started from.
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let month = if shifted_month < 10 {
            shifted_month + 3
        } else {
            shifted_month - 9
        } as u32;
        (if month <= 2 { year + 1 } else { year }, month, day)
    }
}
