//! Generic retry + JSON parsing wrappers around a `reqwest::Response`. Domain
//! crates supply the request builder closure; this module handles 429/5xx
//! retries, the server's own wait headers, transient network failures (timeouts,
//! connection resets), and JSON deserialization with a useful error body.

use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::header::HeaderMap;
use reqwest::{Response, StatusCode};

use super::{api_status, ApiError};

/// Retry tuning for [`execute_with_retry`]. Defaults match what every
/// domain crate in the suite uses today (3 retries, exponential backoff
/// starting at 1s).
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Max number of retries after the first attempt. Total attempts is
    /// `max_retries + 1`.
    pub max_retries: u32,
    /// Backoff factor in seconds. The delay between attempt `n` and `n+1`
    /// is `base_backoff_secs * 2^n` unless the response says how long to wait
    /// (`retry-after` or `x-ratelimit-reset`, which then win).
    pub base_backoff_secs: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_backoff_secs: 1,
        }
    }
}

impl RetryPolicy {
    fn delay_for(&self, attempt: u32, retry_after: Option<u64>) -> Duration {
        let secs = retry_after.unwrap_or_else(|| self.base_backoff_secs << attempt);
        Duration::from_secs(secs)
    }
}

/// The longest wait the server can ask for before the command gives up instead.
///
/// A limit with a long window would otherwise look like a hang. Saying how long
/// it is and stopping leaves the choice with whoever is running the command.
const MAX_SERVER_WAIT_SECS: u64 = 120;

/// A wait this long gets a line on stderr, so it does not read as a hang.
const ANNOUNCE_WAIT_SECS: u64 = 5;

/// How long the response itself says to wait, in seconds.
///
/// On a 429, the larger of `retry-after` and `x-ratelimit-reset`. Trusting
/// `retry-after` alone is wrong: measured on a Roblox route with a 60 second
/// window, a 429 carried `Retry-After: 5` and `x-ratelimit-reset: 51`, and the
/// route reopened after 52 seconds. Retrying on the shorter figure spends every
/// attempt inside the window. The larger one rather than the reset alone, so a
/// route where `retry-after` is the longer of the two is still respected.
///
/// The reset gets one second on top because it counts whole seconds and the
/// window closes after it, which is what that measurement showed.
///
/// Other statuses look at `retry-after` only: the reset describes the rate
/// limit, and has nothing to say about a 503.
///
/// RFC 9110 also allows `retry-after` as an HTTP-date. Only the seconds form is
/// parsed, and a date falls through to the exponential backoff. Roblox sends
/// seconds, so parsing dates would buy a dependency and a clock-skew question
/// for nothing.
fn server_wait_secs(status: StatusCode, headers: &HeaderMap) -> Option<u64> {
    let seconds = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
    };
    let retry_after = seconds("retry-after");
    if status != StatusCode::TOO_MANY_REQUESTS {
        return retry_after;
    }
    let reset = seconds("x-ratelimit-reset").map(|s| s.saturating_add(1));
    retry_after.max(reset)
}

/// Execute an HTTP request, retrying on transient failures with the default
/// retry policy. See [`execute_with_retry_policy`] for control.
pub async fn execute_with_retry<F, Fut>(make_request: F) -> Result<Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Response>>,
{
    execute_with_retry_policy(make_request, &RetryPolicy::default()).await
}

/// Same as [`execute_with_retry`] but with a configurable policy. Retries on:
/// - HTTP 429 (Too Many Requests) and 5xx server errors
/// - Reqwest network errors classified as transient (timeout, connect)
///
/// Does **not** retry on 4xx other than 429, or on opaque errors from the
/// request closure that aren't reqwest network errors (e.g. config errors).
/// A connection that dropped once the request was on it is not retried
/// either: the request reached Roblox.
///
/// For reads, and for writes whose second arrival is accepted by the caller:
/// a `DELETE`, which after a timeout may answer 404 for a delete that went
/// through, or a `PUT` that sets a value outright. A write that must not land
/// twice goes through [`execute_create_with_retry_policy`] instead.
pub async fn execute_with_retry_policy<F, Fut>(
    make_request: F,
    policy: &RetryPolicy,
) -> Result<Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Response>>,
{
    execute(make_request, policy, Resend::AnyTransientFailure).await
}

/// For a request that must not land twice: retries on HTTP 429 and on a
/// failure to connect, and on nothing else.
///
/// A 429 is Roblox refusing before doing anything, and a failure to connect
/// sent nothing, so sending again cannot apply the request a second time. A
/// 5xx, a timeout once connected, or any other transport failure after the
/// request left carries no such promise: the create may have gone through
/// before the answer was lost, and resending it makes a second developer
/// product or pass that cannot be deleted. Those come back to the caller on
/// the first occurrence. The two sides are the ones [`may_have_landed`] draws.
pub async fn execute_create_with_retry_policy<F, Fut>(
    make_request: F,
    policy: &RetryPolicy,
) -> Result<Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Response>>,
{
    execute(make_request, policy, Resend::OnlyWhatCannotHaveLanded).await
}

/// Whether a write that failed may still have been applied.
///
/// Roblox refusing is an answer: a 4xx, a 429 included, means nothing
/// happened. A 5xx is no answer, and neither is a transport failure once the
/// request has left: either can hide a write that went through. A failure to
/// connect is the one transport failure that cannot, because nothing was sent.
///
/// An answer turned into an error on the caller's side is still an answer: a
/// status from `error_for_status` (a 5xx excepted, as above), a redirect the
/// client's policy refused, a body that could not be decoded.
pub fn may_have_landed(error: &anyhow::Error) -> bool {
    if api_status(error).is_some_and(|status| status.is_server_error()) {
        return true;
    }
    reached(error) == Some(Reached::Unknown)
}

/// How far a failed request got, as its `reqwest::Error` tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reached {
    /// The request could not be built, so nothing was sent.
    NotBuilt,
    /// No connection was made, so nothing was sent.
    NotConnected,
    /// Roblox answered, and the answer was turned into an error here.
    Answered,
    /// The request left, and no answer says what became of it.
    Unknown,
}

/// The first `reqwest::Error` in `error`'s chain, read as [`Reached`]. `None`
/// when the chain holds none, which is an error of the caller's own.
fn reached(error: &anyhow::Error) -> Option<Reached> {
    let sent = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())?;
    // A connect timeout is a timeout too: the connection test comes first.
    Some(if sent.is_builder() {
        Reached::NotBuilt
    } else if sent.is_connect() {
        Reached::NotConnected
    } else if sent.is_status() {
        if sent.status().is_some_and(|status| status.is_server_error()) {
            Reached::Unknown
        } else {
            Reached::Answered
        }
    } else if sent.is_redirect() || sent.is_decode() {
        Reached::Answered
    } else {
        Reached::Unknown
    })
}

/// Attach `advice` to a failed write that may still have been applied.
///
/// For a write that must not land twice, sent through
/// [`execute_create_with_retry_policy`]. It is not sent again, and whoever
/// reads the error has to be told not to send it again either before checking:
/// `advice` says how. A refusal and a failure to connect pass through as they
/// are, since neither leaves anything to check.
pub fn unconfirmed(error: anyhow::Error, advice: &str) -> anyhow::Error {
    if may_have_landed(&error) {
        error.context(advice.to_string())
    } else {
        error
    }
}

/// Which failures are safe to send again.
#[derive(Clone, Copy, PartialEq)]
enum Resend {
    AnyTransientFailure,
    /// A 429, or a failure to connect.
    OnlyWhatCannotHaveLanded,
}

impl Resend {
    /// Whether an answer with this status is sent again.
    fn after_status(self, status: StatusCode) -> bool {
        status == StatusCode::TOO_MANY_REQUESTS
            || (self == Self::AnyTransientFailure && status.is_server_error())
    }

    /// Whether a request that failed with `error` is sent again.
    fn after_error(self, error: &anyhow::Error) -> bool {
        match self {
            Self::AnyTransientFailure => is_transient_network_error(error),
            Self::OnlyWhatCannotHaveLanded => reached(error) == Some(Reached::NotConnected),
        }
    }
}

async fn execute<F, Fut>(
    mut make_request: F,
    policy: &RetryPolicy,
    resend: Resend,
) -> Result<Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Response>>,
{
    let mut attempt = 0;
    loop {
        match make_request().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Ok(response);
                }

                if !resend.after_status(status) || attempt >= policy.max_retries {
                    let body = response.text().await.unwrap_or_default();
                    // Typed rather than formatted: callers branch on the
                    // status, and a body is free to contain digits that look
                    // like one. See `super::error`.
                    return Err(ApiError::new(status, body).into());
                }

                let server_wait = server_wait_secs(status, response.headers());
                if let Some(wait) = server_wait.filter(|w| *w > MAX_SERVER_WAIT_SECS) {
                    let body = response.text().await.unwrap_or_default();
                    return Err(anyhow::Error::new(ApiError::new(status, body))
                        .context(too_long_to_wait(status, wait)));
                }

                let delay = policy.delay_for(attempt, server_wait);
                if delay.as_secs() >= ANNOUNCE_WAIT_SECS {
                    eprintln!("{}", wait_notice(status, delay.as_secs()));
                }
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => {
                if !resend.after_error(&e) || attempt >= policy.max_retries {
                    return Err(e);
                }
                tokio::time::sleep(policy.delay_for(attempt, None)).await;
                attempt += 1;
            }
        }
    }
}

/// The line a long wait before a retry prints, saying what Roblox answered.
///
/// A 429 is a rate limit. A 5xx is Roblox answering with an error, and calling
/// it a rate limit would send whoever reads it looking at their request rate.
fn wait_notice(status: StatusCode, secs: u64) -> String {
    if status == StatusCode::TOO_MANY_REQUESTS {
        format!("Rate limited by Roblox, waiting {secs}s...")
    } else {
        format!(
            "Roblox answered {}, retrying in {secs}s...",
            status_text(status)
        )
    }
}

/// `503 Service Unavailable`, or the bare code for a status with no standard
/// name: `StatusCode`'s own `Display` prints `<unknown status code>` for the
/// 520 and 522 a CDN in front of Roblox answers with.
fn status_text(status: StatusCode) -> String {
    match status.canonical_reason() {
        Some(reason) => format!("{} {reason}", status.as_u16()),
        None => status.as_u16().to_string(),
    }
}

/// Why a retry is given up when the wait Roblox asks for is too long.
fn too_long_to_wait(status: StatusCode, wait: u64) -> String {
    let asked = if status == StatusCode::TOO_MANY_REQUESTS {
        format!("Roblox says this limit reopens in {wait}s")
    } else {
        format!(
            "Roblox answered {} and asks to wait {wait}s before trying again",
            status_text(status)
        )
    };
    format!("{asked}, which is too long to wait here. Run the same command again after that.")
}

/// Whether a request-closure error is something we should retry. Walks the
/// anyhow source chain looking for a `reqwest::Error` that's a timeout or a
/// connect failure (the two flavours of transient network flake we see most
/// often against Roblox's edges).
fn is_transient_network_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(req_err) = cause.downcast_ref::<reqwest::Error>() {
            if req_err.is_timeout() || req_err.is_connect() {
                return true;
            }
        }
    }
    false
}

/// Same as [`execute_with_retry`] but also parses the body as JSON into `T`.
/// The body is fetched as text first so deserialization errors include the
/// raw response, which makes Open Cloud's frequent schema drift easier to
/// diagnose.
///
/// The retries cover sending the request and the status it is answered with.
/// Reading the body comes after them and is not retried: a connection that
/// drops while the body arrives is returned as it is.
pub async fn execute_json<T, F, Fut>(make_request: F) -> Result<T>
where
    T: serde::de::DeserializeOwned,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Response>>,
{
    parse_json(execute_with_retry(make_request).await?).await
}

/// Read `response`'s body and parse it as JSON, with the raw body in the error
/// when it does not parse. Not retried, like the body read in [`execute_json`].
pub(super) async fn parse_json<T: serde::de::DeserializeOwned>(response: Response) -> Result<T> {
    let body = response.text().await?;
    serde_json::from_str(&body)
        .map_err(|e| anyhow!("Failed to parse response: {}\nBody: {}", e, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_3_retries_1s_base() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_retries, 3);
        assert_eq!(p.base_backoff_secs, 1);
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    const TOO_MANY: StatusCode = StatusCode::TOO_MANY_REQUESTS;

    /// The headers of a real 429: the short figure is the wrong one.
    #[test]
    fn a_429_waits_for_the_reset_when_retry_after_is_shorter() {
        let h = headers(&[("retry-after", "5"), ("x-ratelimit-reset", "51")]);
        assert_eq!(server_wait_secs(TOO_MANY, &h), Some(52));
    }

    #[test]
    fn a_429_keeps_retry_after_when_it_is_the_longer_one() {
        let h = headers(&[("retry-after", "30"), ("x-ratelimit-reset", "3")]);
        assert_eq!(server_wait_secs(TOO_MANY, &h), Some(30));
    }

    #[test]
    fn a_429_with_one_header_or_none_uses_what_is_there() {
        let reset = headers(&[("x-ratelimit-reset", "10")]);
        assert_eq!(server_wait_secs(TOO_MANY, &reset), Some(11));
        let after = headers(&[("retry-after", "7")]);
        assert_eq!(server_wait_secs(TOO_MANY, &after), Some(7));
        assert_eq!(server_wait_secs(TOO_MANY, &headers(&[])), None);
    }

    /// The reset is on every response, including a 503 from an unrelated
    /// failure, where it says nothing about when to try again.
    #[test]
    fn a_5xx_ignores_the_reset() {
        let h = headers(&[("x-ratelimit-reset", "51")]);
        assert_eq!(server_wait_secs(StatusCode::SERVICE_UNAVAILABLE, &h), None);
    }

    /// A date is a legal `retry-after`, and falls back to the backoff.
    #[test]
    fn a_retry_after_date_is_not_a_number_of_seconds() {
        let h = headers(&[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")]);
        assert_eq!(server_wait_secs(TOO_MANY, &h), None);
    }

    /// A 5xx is not a rate limit, and the line a wait prints does not call it
    /// one.
    #[test]
    fn a_wait_says_what_roblox_answered() {
        assert_eq!(
            wait_notice(TOO_MANY, 52),
            "Rate limited by Roblox, waiting 52s..."
        );
        let after_a_503 = wait_notice(StatusCode::SERVICE_UNAVAILABLE, 8);
        assert_eq!(
            after_a_503,
            "Roblox answered 503 Service Unavailable, retrying in 8s..."
        );
    }

    /// A status with no standard name is printed as its code alone.
    #[test]
    fn a_status_with_no_name_is_printed_as_its_code() {
        let cdn = StatusCode::from_u16(520).unwrap();
        assert_eq!(
            wait_notice(cdn, 8),
            "Roblox answered 520, retrying in 8s..."
        );
        let refusal = too_long_to_wait(StatusCode::from_u16(522).unwrap(), 600);
        assert!(
            refusal.starts_with("Roblox answered 522 and asks"),
            "{refusal}"
        );
    }

    #[test]
    fn a_wait_too_long_says_what_roblox_answered() {
        assert!(too_long_to_wait(TOO_MANY, 3601)
            .starts_with("Roblox says this limit reopens in 3601s,"));
        let after_a_503 = too_long_to_wait(StatusCode::SERVICE_UNAVAILABLE, 600);
        assert!(
            after_a_503.contains("503 Service Unavailable"),
            "{after_a_503}"
        );
        assert!(after_a_503.contains("600s"), "{after_a_503}");
        assert!(!after_a_503.contains("limit"), "{after_a_503}");
    }

    #[test]
    fn delay_uses_retry_after_when_present() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay_for(0, Some(5)), Duration::from_secs(5));
        assert_eq!(p.delay_for(2, Some(10)), Duration::from_secs(10));
    }

    #[test]
    fn delay_uses_exponential_backoff_when_no_header() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay_for(0, None), Duration::from_secs(1));
        assert_eq!(p.delay_for(1, None), Duration::from_secs(2));
        assert_eq!(p.delay_for(2, None), Duration::from_secs(4));
        assert_eq!(p.delay_for(3, None), Duration::from_secs(8));
    }

    #[test]
    fn delay_scales_with_base_backoff() {
        let p = RetryPolicy {
            max_retries: 3,
            base_backoff_secs: 2,
        };
        assert_eq!(p.delay_for(0, None), Duration::from_secs(2));
        assert_eq!(p.delay_for(1, None), Duration::from_secs(4));
        assert_eq!(p.delay_for(2, None), Duration::from_secs(8));
    }
}
