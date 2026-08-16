//! Generic retry + JSON parsing wrappers around a `reqwest::Response`. Domain
//! crates supply the request builder closure; this module handles 429/5xx
//! retries, `retry-after` parsing, transient network failures (timeouts,
//! connection resets), and JSON deserialization with a useful error body.

use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::{Response, StatusCode};

use super::ApiError;

/// Retry tuning for [`execute_with_retry`]. Defaults match what every
/// domain crate in the suite uses today (3 retries, exponential backoff
/// starting at 1s).
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Max number of retries after the first attempt. Total attempts is
    /// `max_retries + 1`.
    pub max_retries: u32,
    /// Backoff factor in seconds. The delay between attempt `n` and `n+1`
    /// is `base_backoff_secs * 2^n` unless the server sent a `retry-after`
    /// header (in which case that value wins).
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
pub async fn execute_with_retry_policy<F, Fut>(
    mut make_request: F,
    policy: &RetryPolicy,
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

                let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                if !retryable || attempt >= policy.max_retries {
                    let body = response.text().await.unwrap_or_default();
                    // Typed rather than formatted: callers branch on the
                    // status, and a body is free to contain digits that look
                    // like one. See `super::error`.
                    return Err(ApiError::new(status, body).into());
                }

                // RFC 9110 allows `retry-after` as either delta-seconds or an
                // HTTP-date. Only the seconds form is parsed; a date falls
                // through to `unwrap_or_else` and gets the exponential
                // backoff. Roblox sends seconds, and the fallback is a correct
                // (if less polite) answer to a date, so parsing dates would
                // buy a dependency and a clock-skew question for nothing.
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());
                tokio::time::sleep(policy.delay_for(attempt, retry_after)).await;
                attempt += 1;
            }
            Err(e) => {
                if !is_transient_network_error(&e) || attempt >= policy.max_retries {
                    return Err(e);
                }
                tokio::time::sleep(policy.delay_for(attempt, None)).await;
                attempt += 1;
            }
        }
    }
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
pub async fn execute_json<T, F, Fut>(make_request: F) -> Result<T>
where
    T: serde::de::DeserializeOwned,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Response>>,
{
    let response = execute_with_retry(make_request).await?;
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
