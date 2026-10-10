//! Request-side protections for the control API: who may talk to it, how big a body it accepts,
//! and how fast it may be driven.
//!
//! Everything here is opt-in or deliberately generous. loadlab is a loopback tool by default, and
//! the failure mode of a security control that gets in the operator's way is that it gets turned
//! off — so a control that only slows someone down is worse than no control, because it looks like
//! protection.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{Request, State},
    http::{HeaderMap, Method},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::error::LoadlabError;

/// Largest request body accepted, in bytes.
///
/// Every body this API takes is a workload description: a few kilobytes of JSON at the very most,
/// with expressions and pattern parameters. Anything approaching this is not a workload. The limit
/// exists so an unauthenticated caller cannot make the process buffer an arbitrary amount of
/// memory, which on this machine would compete with the very workload it is supposed to be
/// measuring.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Sustained mutations per second, and the burst that may be spent at once.
///
/// Sized so an interactive session driving the workload by hand cannot notice it, while a client
/// stuck in a loop cannot turn configuration churn into load. Reads are not limited: they are
/// cheap, they do not change state, and a scrape in a loop is a legitimate thing to do.
pub const MUTATION_RATE_PER_SECOND: f64 = 20.0;

/// Burst allowance, in mutations. A client that has been idle spends this much at once.
pub const MUTATION_BURST: f64 = 40.0;

/// A closed token bucket, refilled at [`MUTATION_RATE_PER_SECOND`] up to [`MUTATION_BURST`].
///
/// One bucket for the whole API rather than one per client. A per-client limit would be more
/// precise, but the thing being protected is this single process: what matters is that the total
/// rate of mutation is bounded, and a global bucket says that in five lines. It also cannot be
/// defeated by reconnecting, which is the first thing anyone would try against a per-address limit.
#[derive(Debug)]
pub struct RateLimiter {
    state: Mutex<Bucket>,
    /// Tokens earned per second.
    rate_per_second: f64,
    /// Ceiling on the tokens the bucket can hold, which is also what it starts full at.
    burst: f64,
}

#[derive(Debug)]
struct Bucket {
    /// Tokens currently available.
    tokens: f64,
    /// When the bucket was last refilled, so the refill is computed from elapsed time rather than
    /// accumulated per request.
    last_refill: Instant,
}

impl RateLimiter {
    fn new(rate_per_second: f64, burst: f64) -> Self {
        Self {
            state: Mutex::new(Bucket {
                tokens: burst,
                last_refill: Instant::now(),
            }),
            rate_per_second,
            burst,
        }
    }

    /// Spend one token, returning how long the caller should wait if it could not.
    fn check(&self) -> Result<(), Duration> {
        let mut bucket = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        // Capped so a long pause does not mint more than a burst: an hour of idle time is not an
        // hour of credit.
        bucket.tokens = (bucket.tokens + elapsed * self.rate_per_second).min(self.burst);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        let deficit = 1.0 - bucket.tokens;
        Err(Duration::from_secs_f64(deficit / self.rate_per_second).max(
            // Never answer `Retry-After: 0`, which is indistinguishable from not limiting at all.
            Duration::from_millis(1),
        ))
    }
}

/// Require a bearer token, when one is configured.
///
/// With no token configured the API is open, which is the loopback default: an operator running
/// loadlab on their own machine should not have to invent a secret to use it. Configuring a token
/// is what makes it safe to expose beyond loopback.
///
/// The token is also accepted as `?token=` so a Prometheus scrape can be configured without
/// credentials support in the exporter. That is a weaker channel than the header — URLs end up in
/// proxy logs and in `Referer` — so it is refused when a token is set and the request came from
/// somewhere other than loopback, where it has no exposure advantage over a header anyway.
pub async fn require_token(
    State(security): State<Security>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = security.token.as_deref() else {
        return next.run(request).await;
    };

    if let Some(presented) = bearer_token(request.headers()) {
        if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            return next.run(request).await;
        }
        return LoadlabError::Unauthorized.into_response();
    }

    // No header. The query parameter is the remaining option, and only from loopback.
    if let Some(query) = request.uri().query() {
        if let Some(value) = query_parameter(query, "token") {
            let from_loopback = request
                .extensions()
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .is_some_and(|peer| peer.ip().is_loopback());
            if from_loopback && constant_time_eq(value.as_bytes(), expected.as_bytes()) {
                return next.run(request).await;
            }
        }
    }

    LoadlabError::Unauthorized.into_response()
}

/// Reject an oversized body before a handler ever sees it.
///
/// `Content-Length` is what a client declares, and a declared length is the cheapest thing to act
/// on: no bytes are read, no buffer is allocated, and the answer is immediate. It is also a claim
/// rather than a fact, so this is a fast path and not the limit itself — the router's body limit
/// enforces the same number for a body that lies about its length or omits it entirely.
pub async fn limit_body_size(max_bytes: usize, request: Request, next: Next) -> Response {
    let declared = request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > max_bytes as u64) {
        return LoadlabError::PayloadTooLarge { limit: max_bytes }.into_response();
    }
    next.run(request).await
}

/// Reject a mutating request once the rate limiter is out of tokens.
pub async fn limit_mutation_rate(
    State(security): State<Security>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(
        *request.method(),
        Method::PUT | Method::POST | Method::PATCH | Method::DELETE
    ) {
        if let Err(wait) = security.mutation_rate.check() {
            // Rounded up, because a client that sleeps for a fraction of a second and is then
            // refused again has learned nothing and is back here immediately.
            let seconds = wait.as_secs().max(1);
            return LoadlabError::TooManyRequests {
                retry_after_seconds: seconds,
            }
            .into_response();
        }
    }
    next.run(request).await
}

/// The per-process protections every route shares.
#[derive(Clone, Debug)]
pub struct Security {
    /// The configured token, or `None` for an open API.
    token: Option<Arc<str>>,
    mutation_rate: Arc<RateLimiter>,
}

impl Security {
    /// Build from the operator's configuration.
    ///
    /// A blank token is treated as no token, because an empty value is what an unset environment
    /// variable looks like, and requiring a secret that is the empty string is not a security
    /// control.
    ///
    /// A rate of zero or less, or a burst below one, would mean refusing every mutation forever.
    /// That is a configuration mistake rather than a policy anyone can reach by accident, so it is
    /// reported rather than quietly corrected into something that looks like it is working.
    pub fn new(
        token: Option<String>,
        mutation_rate_per_second: f64,
        burst: f64,
    ) -> Result<Self, String> {
        if !mutation_rate_per_second.is_finite() || mutation_rate_per_second <= 0.0 {
            return Err(format!(
                "--mutation-rate-per-second must be a positive number of seconds, got {mutation_rate_per_second}"
            ));
        }
        if !burst.is_finite() || burst < 1.0 {
            return Err(format!("--mutation-burst must be at least 1, got {burst}"));
        }

        let token = token
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .map(Arc::from);
        Ok(Self {
            token,
            mutation_rate: Arc::new(RateLimiter::new(mutation_rate_per_second, burst)),
        })
    }

    /// Whether a token is required. Reported at startup so an exposed API is never a surprise.
    pub fn is_open(&self) -> bool {
        self.token.is_none()
    }
}

/// The credential from an `Authorization: Bearer …` header, if the scheme matches.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = value.split_once(' ')?;
    // Scheme names are case-insensitive per RFC 7235, so `bearer` has to work as well as `Bearer`.
    // The token itself may not contain whitespace: RFC 6750 defines it as a single b64token, and
    // accepting spaces would make `Authorization: Bearer a b` a *different* token from `bearer` and
    // `a` depending on how a client happened to build the header.
    let token = token.trim();
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.to_owned())
        .filter(|token| !token.is_empty() && !token.contains(char::is_whitespace))
}

/// One `name=value` pair out of a raw query string.
///
/// Hand-rolled rather than pulled in through the `url` crate: this runs on the unauthenticated
/// path of every request, and it only ever has to read a flat parameter.
fn query_parameter(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name && !value.is_empty()).then(|| percent_decode(value))
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                } else {
                    // Not a valid escape: keep the literal `%`, because a token containing one
                    // should fail to match rather than silently become something else.
                    out.push(b'%');
                    index += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Compare two byte strings without leaking where they first differ.
///
/// A byte-at-a-time comparison returns as soon as it finds a mismatch, so the time it takes says
/// how many leading bytes were correct. Over enough attempts that is enough to recover a token one
/// byte at a time, which is why the length check here is not optional: it also stops the loop from
/// running off the end of the shorter input.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(value).expect("a header value"),
        );
        headers
    }

    #[test]
    fn a_bearer_token_is_read_from_the_header() {
        assert_eq!(
            bearer_token(&header("Bearer secret")).as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn the_scheme_is_case_insensitive_but_the_rest_is_not() {
        assert_eq!(
            bearer_token(&header("bearer secret")).as_deref(),
            Some("secret")
        );
        assert_eq!(
            bearer_token(&header("BEARER secret")).as_deref(),
            Some("secret")
        );
        // A token is compared byte for byte, so its case has to survive the round trip.
        assert_eq!(
            bearer_token(&header("Bearer SeCrEt")).as_deref(),
            Some("SeCrEt")
        );
    }

    #[test]
    fn a_malformed_authorization_header_is_not_a_token() {
        for value in [
            "Bearer",
            "Bearer   ",
            "Basic dXNlcjpwYXNz",
            "secret",
            "Bearer a b",
            "",
        ] {
            assert_eq!(
                bearer_token(&header(value)),
                None,
                "{value:?} was read as a token"
            );
        }
    }

    #[test]
    fn the_token_parameter_is_found_in_a_flat_query_string() {
        assert_eq!(
            query_parameter("token=secret", "token").as_deref(),
            Some("secret")
        );
        assert_eq!(
            query_parameter("a=1&token=secret&b=2", "token").as_deref(),
            Some("secret")
        );
        assert_eq!(query_parameter("tokens=secret", "token"), None);
        assert_eq!(query_parameter("token=", "token"), None);
        assert_eq!(query_parameter("", "token"), None);
        assert_eq!(query_parameter("a=1", "token"), None);
    }

    #[test]
    fn percent_and_plus_escapes_are_decoded() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        // An invalid escape is kept verbatim rather than dropped, so it cannot change what a
        // literal token means.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
    }

    #[test]
    fn comparison_agrees_with_equality_and_not_with_the_rest() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"secret", b"secrez"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"secret", b"secrets"));
        assert!(!constant_time_eq(b"Secret", b"secret"));
    }

    #[test]
    fn the_bucket_allows_a_burst_and_then_refuses_until_it_refills() {
        let limiter = RateLimiter::new(MUTATION_RATE_PER_SECOND, MUTATION_BURST);
        for _ in 0..MUTATION_BURST as usize {
            limiter.check().expect("within the burst allowance");
        }
        let wait = limiter
            .check()
            .expect_err("the burst is spent and the rate has to bite");
        assert!(wait > Duration::ZERO, "{wait:?}");
        // Long enough to be worth a client respecting, short enough to be unnoticeable.
        assert!(wait <= Duration::from_secs(1), "{wait:?}");
    }

    #[test]
    fn the_bucket_refills_over_time() {
        let limiter = RateLimiter::new(MUTATION_RATE_PER_SECOND, MUTATION_BURST);
        for _ in 0..MUTATION_BURST as usize {
            limiter.check().expect("within the burst allowance");
        }
        limiter.check().expect_err("the burst is spent");

        std::thread::sleep(Duration::from_millis(200));
        // 200ms at 20/s is about four tokens. The lower bound is what matters: a bucket that did not
        // refill would refuse everything, which is the failure this guards. The upper bound is loose
        // because a loaded machine can overshoot the sleep, and overshooting only adds tokens.
        let mut allowed = 0;
        for _ in 0..=MUTATION_BURST as usize {
            if limiter.check().is_err() {
                break;
            }
            allowed += 1;
        }
        assert!(
            allowed >= 2,
            "200ms at 20/s should refill several tokens, got {allowed}"
        );
        assert!(
            allowed <= MUTATION_BURST as usize,
            "refill exceeded the burst allowance: {allowed}"
        );
    }

    #[test]
    fn an_idle_api_does_not_accumulate_credit() {
        let limiter = RateLimiter::new(MUTATION_RATE_PER_SECOND, MUTATION_BURST);
        // Backdated directly rather than by sleeping: a minute of apparent idleness is ten bursts'
        // worth of time, and the cap is what has to stop it being worth anything. Sleeping for a
        // minute to prove it is not a reasonable thing to ask of a test suite.
        {
            let mut bucket = limiter
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            bucket.tokens = 0.0;
            let an_hour_ago = Instant::now().checked_sub(Duration::from_secs(60));
            if let Some(an_hour_ago) = an_hour_ago {
                bucket.last_refill = an_hour_ago;
            }
        }

        let mut allowed = 0;
        for _ in 0..=MUTATION_BURST as usize {
            if limiter.check().is_err() {
                break;
            }
            allowed += 1;
        }
        assert_eq!(
            allowed, MUTATION_BURST as usize,
            "an idle minute must mint exactly one burst, no more"
        );
    }

    fn security_with(token: Option<&str>) -> Security {
        Security::new(
            token.map(str::to_owned),
            MUTATION_RATE_PER_SECOND,
            MUTATION_BURST,
        )
        .expect("a valid configuration")
    }

    #[test]
    fn a_blank_token_leaves_the_api_open() {
        assert!(security_with(None).is_open());
        assert!(security_with(Some("")).is_open());
        assert!(security_with(Some("   ")).is_open());
        assert!(!security_with(Some("secret")).is_open());
    }

    #[test]
    fn a_configured_token_is_trimmed_so_a_trailing_newline_does_not_lock_the_operator_out() {
        let security = security_with(Some("  secret\n"));
        assert!(!security.is_open());
        let expected = security.token.as_deref().expect("a token");
        assert_eq!(expected, "secret");
    }

    #[test]
    fn a_limit_that_would_refuse_everything_is_refused_at_startup() {
        // Silently correcting these would look like the limiter is working when it is refusing
        // every write, which is the hardest kind of misconfiguration to notice.
        for (rate, burst) in [
            (0.0, MUTATION_BURST),
            (-1.0, MUTATION_BURST),
            (f64::NAN, 1.0),
        ] {
            assert!(
                Security::new(None, rate, burst).is_err(),
                "a rate of {rate} was accepted"
            );
        }
        for burst in [0.0, 0.5] {
            assert!(
                Security::new(None, MUTATION_RATE_PER_SECOND, burst).is_err(),
                "a burst of {burst} was accepted"
            );
        }
        assert!(Security::new(None, 0.5, 1.0).is_ok());
    }
}
