#![allow(clippy::unwrap_used)]
//! Integration tests for the retry logic, driven by an in-process wiremock
//! server. We use a tiny `RetryPolicy` (zero backoff) to keep tests instant.

use anyhow::Result;
use rbx_core::api::{execute_json, execute_with_retry_policy, RetryPolicy};
use reqwest::Client;
use serde::Deserialize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        max_retries: 3,
        base_backoff_secs: 0,
    }
}

fn fast_policy_n(max_retries: u32) -> RetryPolicy {
    RetryPolicy {
        max_retries,
        base_backoff_secs: 0,
    }
}

/// Client timeout for the tests that need one. Generous rather than tight:
/// these tests assert *which* response came back, not how fast, so the only
/// job of the timeout is to sit between a deliberately slow response and a
/// normal one. Under a millisecond of real work, a busy CI runner is the one
/// thing that can push a "normal" response past a tight bound.
const CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

async fn get(client: &Client, url: &str) -> Result<reqwest::Response> {
    Ok(client.get(url).send().await?)
}

#[derive(Debug, Deserialize, PartialEq)]
struct Payload {
    name: String,
    n: u32,
}

#[tokio::test]
async fn success_first_try_no_retry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::new();
    let url = format!("{}/ok", server.uri());
    let resp = execute_with_retry_policy(|| get(&client, &url), &fast_policy())
        .await
        .unwrap();
    assert!(resp.status().is_success());
}

#[tokio::test]
async fn retry_on_429_then_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/r"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/r"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::new();
    let url = format!("{}/r", server.uri());
    let resp = execute_with_retry_policy(|| get(&client, &url), &fast_policy())
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn retry_on_429_with_retry_after_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/r"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/r"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::new();
    let url = format!("{}/r", server.uri());
    let resp = execute_with_retry_policy(|| get(&client, &url), &fast_policy())
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn retry_on_5xx_then_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/r"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/r"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::new();
    let url = format!("{}/r", server.uri());
    let resp = execute_with_retry_policy(|| get(&client, &url), &fast_policy())
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn bail_on_5xx_after_max_retries() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicU32::new(0));
    let calls_clone = calls.clone();

    // Always return 500.
    Mock::given(method("GET"))
        .and(path("/r"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream is on fire"))
        .mount(&server)
        .await;

    let client = Client::new();
    let url = format!("{}/r", server.uri());
    let err = execute_with_retry_policy(
        || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            get(&client, &url)
        },
        &fast_policy_n(2),
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(err.contains("500"), "error should mention status: {err}");
    assert!(
        err.contains("upstream is on fire"),
        "error should include body: {err}"
    );
    // 1 initial attempt + 2 retries = 3.
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn no_retry_on_4xx_other_than_429() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicU32::new(0));
    let calls_clone = calls.clone();

    Mock::given(method("GET"))
        .and(path("/r"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::new();
    let url = format!("{}/r", server.uri());
    let err = execute_with_retry_policy(
        || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            get(&client, &url)
        },
        &fast_policy(),
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(err.contains("400"));
    assert!(err.contains("bad request"));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "should not retry on 400");
}

#[tokio::test]
async fn execute_json_parses_valid_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/j"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"name":"vip","n":42}"#, "application/json"),
        )
        .mount(&server)
        .await;

    let client = Client::new();
    let url = format!("{}/j", server.uri());
    let p: Payload = execute_json(|| get(&client, &url)).await.unwrap();
    assert_eq!(
        p,
        Payload {
            name: "vip".into(),
            n: 42
        }
    );
}

#[tokio::test]
async fn execute_json_error_includes_raw_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/j"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("not json at all", "text/plain"))
        .mount(&server)
        .await;

    let client = Client::new();
    let url = format!("{}/j", server.uri());
    let err = execute_json::<Payload, _, _>(|| get(&client, &url))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not json at all"),
        "error should embed raw body: {err}"
    );
    assert!(err.contains("Failed to parse"));
}

#[tokio::test]
async fn retry_on_network_timeout_then_succeed_on_real_request() {
    // wiremock cannot drop a connection on demand, so a timeout stands in for
    // the transient network failure: the first response is delayed past the
    // client's timeout, the second is immediate.
    //
    // The two durations are 20x apart on purpose. The assertion only holds
    // while "delayed" and "immediate" stay on opposite sides of the timeout,
    // and on a loaded CI runner the *immediate* response can be late too. A
    // narrow margin turns that into a flake that looks like a retry bug.

    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_clone = attempts.clone();

    Mock::given(method("GET"))
        .and(path("/n"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("late")
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/n"))
        .respond_with(ResponseTemplate::new(200).set_body_string("fast"))
        .expect(1)
        .mount(&server)
        .await;

    // Long enough that a stalled runner does not make the fast response look
    // slow; short enough that the deliberately slow one still times out.
    let client = Client::builder().timeout(CLIENT_TIMEOUT).build().unwrap();
    let url = format!("{}/n", server.uri());

    let resp = execute_with_retry_policy(
        || {
            attempts_clone.fetch_add(1, Ordering::SeqCst);
            let c = client.clone();
            let u = url.clone();
            async move { Ok(c.get(&u).send().await?) }
        },
        &fast_policy_n(2),
    )
    .await
    .unwrap();

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(body, "fast", "should have used the second (fast) response");
    assert!(
        attempts.load(Ordering::SeqCst) >= 2,
        "should have retried at least once after timeout"
    );
}

#[tokio::test]
async fn connect_errors_are_retried_until_the_budget_is_exhausted() {
    // A connect error to a dead port is *transient*, so this is the giving-up
    // path, not the no-retry path: the budget is spent and then the error
    // bubbles. This used to be called `no_retry_on_non_transient_error`, which
    // described the opposite of what it does: the two tests below cover the
    // case that name promised.
    let calls = Arc::new(AtomicU32::new(0));
    let calls_clone = calls.clone();

    let result = execute_with_retry_policy(
        || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            async {
                // Unreachable port -> reqwest connect error
                Ok(Client::new()
                    .get("http://127.0.0.1:1/never")
                    .timeout(CLIENT_TIMEOUT)
                    .send()
                    .await?)
            }
        },
        &fast_policy_n(2),
    )
    .await;

    assert!(result.is_err(), "should bubble up the network error");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "1 initial + 2 retries: connect errors are transient, so the whole budget is spent"
    );
}

#[tokio::test]
async fn a_reqwest_error_that_is_not_a_network_flake_is_not_retried() {
    // The case the old name of the test above promised. The classifier walks
    // the source chain for a `reqwest::Error` and retries only `is_timeout()`
    // or `is_connect()`. A malformed URL yields a real `reqwest::Error` that is
    // neither, and retrying it would only fail identically twice more.
    let calls = Arc::new(AtomicU32::new(0));
    let calls_clone = calls.clone();

    let result = execute_with_retry_policy(
        || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            async { Ok(Client::new().get("http://[not-a-url").send().await?) }
        },
        &fast_policy_n(2),
    )
    .await;

    assert!(result.is_err(), "a malformed URL cannot succeed");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a builder error is permanent; the retry budget should go untouched"
    );
}

#[tokio::test]
async fn an_error_that_is_not_a_reqwest_error_at_all_is_not_retried() {
    // The other half of the same rule: an opaque error from the closure (a
    // missing credential, a config failure) carries no `reqwest::Error` to
    // classify and so defaults to permanent.
    let calls = Arc::new(AtomicU32::new(0));
    let calls_clone = calls.clone();

    let result = execute_with_retry_policy(
        || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            async { Err(anyhow::anyhow!("no API key configured")) }
        },
        &fast_policy_n(2),
    )
    .await;

    assert_eq!(
        result.unwrap_err().to_string(),
        "no API key configured",
        "the closure's own error should come back untouched"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "nothing about a config error gets better on a second attempt"
    );
}
