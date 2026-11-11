#![allow(clippy::unwrap_used)]
//! Integration tests for the retry logic, driven by an in-process wiremock
//! server. We use a tiny `RetryPolicy` (zero backoff) to keep tests instant.

use anyhow::Result;
use rbx_core::api::{
    execute_create_with_retry_policy, execute_json, execute_with_retry_policy, is_api_status,
    may_have_landed, unconfirmed, RetryPolicy,
};
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

/// A window far past what is worth sitting through stops at the first answer,
/// keeps the 429 readable to callers that branch on it, and says how long.
#[tokio::test]
async fn a_reset_too_long_to_wait_for_fails_at_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/r"))
        .respond_with(ResponseTemplate::new(429).insert_header("x-ratelimit-reset", "3600"))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::new();
    let url = format!("{}/r", server.uri());
    let err = execute_with_retry_policy(|| get(&client, &url), &fast_policy())
        .await
        .unwrap_err();

    assert!(is_api_status(&err, reqwest::StatusCode::TOO_MANY_REQUESTS));
    assert!(err.to_string().contains("3601s"), "{err}");
}

/// A create may be resent after a 429 because Roblox refused it, and the wait
/// comes from the same place. The reset here is the shortest there is.
#[tokio::test]
async fn a_429_is_retried_after_the_reset() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/r"))
        .respond_with(ResponseTemplate::new(429).insert_header("x-ratelimit-reset", "0"))
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
    let started = std::time::Instant::now();
    let resp = execute_create_with_retry_policy(|| get(&client, &url), &fast_policy())
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
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
async fn a_create_is_resent_after_a_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/c"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/c"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::new();
    let url = format!("{}/c", server.uri());
    let resp = execute_create_with_retry_policy(
        || async { Ok(client.post(&url).send().await?) },
        &fast_policy(),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn a_create_is_not_resent_after_a_5xx() {
    // The product may exist already: a resend would make a second one that
    // Roblox will not let anyone delete.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/c"))
        .respond_with(ResponseTemplate::new(503).set_body_string("try later"))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::new();
    let url = format!("{}/c", server.uri());
    let err = execute_create_with_retry_policy(
        || async { Ok(client.post(&url).send().await?) },
        &fast_policy(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("503"), "{err}");
}

/// The request left and the answer did not come back in time: the create may
/// exist, so it is not sent again.
#[tokio::test]
async fn a_create_is_not_resent_once_the_request_has_left() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(CLIENT_TIMEOUT * 4))
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::builder().timeout(CLIENT_TIMEOUT).build().unwrap();
    let url = format!("{}/slow", server.uri());
    let calls = AtomicU32::new(0);

    let err = execute_create_with_retry_policy(
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(client.post(&url).send().await?) }
        },
        &fast_policy_n(2),
    )
    .await
    .unwrap_err();

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a lost answer may hide a create that went through"
    );
    assert!(may_have_landed(&err), "{err:#}");
}

/// A client that cannot connect: refused at once, or timed out while the
/// connection was still being made. Either way nothing was sent.
fn unconnectable_client() -> Client {
    Client::builder()
        .connect_timeout(CLIENT_TIMEOUT)
        .no_proxy()
        .build()
        .unwrap()
}

/// Nothing was sent, so sending again cannot make a second one: the create is
/// retried, and lands once.
#[tokio::test]
async fn a_create_that_could_not_connect_is_sent_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/c"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let unconnectable = unconnectable_client();
    let client = Client::new();
    let url = format!("{}/c", server.uri());
    let calls = AtomicU32::new(0);

    let resp = execute_create_with_retry_policy(
        || {
            let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
            let (client, url) = if first {
                (&unconnectable, "http://127.0.0.1:1/never")
            } else {
                (&client, url.as_str())
            };
            async move { Ok(client.post(url).send().await?) }
        },
        &fast_policy_n(2),
    )
    .await
    .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// A create that never connects spends the budget, and comes back as one
/// that did not land.
#[tokio::test]
async fn a_create_that_never_connects_spends_the_budget_and_did_not_land() {
    let client = unconnectable_client();
    let calls = AtomicU32::new(0);

    let err = execute_create_with_retry_policy(
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(client.post("http://127.0.0.1:1/never").send().await?) }
        },
        &fast_policy_n(2),
    )
    .await
    .unwrap_err();

    assert_eq!(calls.load(Ordering::SeqCst), 3, "1 initial + 2 retries");
    assert!(!may_have_landed(&err), "{err:#}");
}

/// One write through the create-only path, and the error it came back with.
async fn failed_write(client: &Client, url: &str) -> anyhow::Error {
    execute_create_with_retry_policy(
        || async { Ok(client.post(url).send().await?) },
        &fast_policy_n(0),
    )
    .await
    .unwrap_err()
}

const ADVICE: &str = "read it back before running this again";

/// A 5xx is Roblox not answering, which is not Roblox refusing: the write may
/// have gone through before the answer was lost, and the reader is told.
#[tokio::test]
async fn a_write_answered_with_a_5xx_may_have_landed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/w"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let err = failed_write(&Client::new(), &format!("{}/w", server.uri())).await;

    assert!(may_have_landed(&err), "{err:#}");
    assert!(format!("{:#}", unconfirmed(err, ADVICE)).contains(ADVICE));
}

/// The request left and no answer came back in time: the same doubt.
#[tokio::test]
async fn a_write_whose_answer_never_came_may_have_landed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(CLIENT_TIMEOUT * 4))
        .mount(&server)
        .await;
    let client = Client::builder().timeout(CLIENT_TIMEOUT).build().unwrap();

    let err = failed_write(&client, &format!("{}/slow", server.uri())).await;

    assert!(may_have_landed(&err), "{err:#}");
    assert!(format!("{:#}", unconfirmed(err, ADVICE)).contains(ADVICE));
}

/// A 4xx is an answer, and the answer is no. Telling somebody to go and check
/// for a write Roblox has just refused would send them looking for nothing.
#[tokio::test]
async fn a_write_roblox_refused_did_not_land() {
    for status in [400, 403, 429] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/w"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;

        let err = failed_write(&Client::new(), &format!("{}/w", server.uri())).await;

        assert!(!may_have_landed(&err), "{status}: {err:#}");
        assert!(
            !format!("{:#}", unconfirmed(err, ADVICE)).contains(ADVICE),
            "{status}"
        );
    }
}

/// Nothing was sent, so nothing can have landed, whether the connection was
/// refused at once or timed out while still being made.
#[tokio::test]
async fn a_write_that_never_connected_did_not_land() {
    let client = Client::builder()
        .connect_timeout(CLIENT_TIMEOUT)
        .no_proxy()
        .build()
        .unwrap();

    let err = failed_write(&client, "http://127.0.0.1:1/never").await;

    assert!(!may_have_landed(&err), "{err:#}");
    assert!(!format!("{:#}", unconfirmed(err, ADVICE)).contains(ADVICE));
}

/// An answer that the caller's side turned into an error is still an answer,
/// except a 5xx, which is no answer whichever side reports it, and a redirect
/// the policy refused, which may follow a write that was applied.
#[tokio::test]
async fn an_answer_turned_into_an_error_did_not_land() {
    let server = MockServer::start().await;
    for (route, status) in [("/bad", 400), ("/down", 503)] {
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/loop"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/loop"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/garbled"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::limited(1))
        .build()
        .unwrap();
    let at = |route: &str| format!("{}{route}", server.uri());
    let post = |route: &str| client.post(at(route)).send();

    let refused = post("/bad").await.unwrap().error_for_status().unwrap_err();
    assert!(!may_have_landed(&refused.into()), "a 400 is an answer");

    let down = post("/down").await.unwrap().error_for_status().unwrap_err();
    assert!(may_have_landed(&down.into()), "a 503 is no answer");

    let redirected = post("/loop").await.unwrap_err();
    assert!(redirected.is_redirect(), "{redirected:#}");
    assert!(
        may_have_landed(&redirected.into()),
        "a write redirected too many times"
    );

    let garbled = post("/garbled")
        .await
        .unwrap()
        .json::<Payload>()
        .await
        .unwrap_err();
    assert!(garbled.is_decode(), "{garbled:#}");
    assert!(
        !may_have_landed(&garbled.into()),
        "a body that did not decode"
    );
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

/// A server that reads the request head on every connection and closes it
/// without an answer: the request arrived, and the connection dropped before
/// the reply. Returns the base URL and the count of requests it read.
fn reads_then_closes() -> (String, Arc<AtomicU32>) {
    use std::io::Read;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(AtomicU32::new(0));
    let counted = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(1) => head.push(byte[0]),
                    _ => break,
                }
            }
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });
    (url, seen)
}

/// The request reached the server, so sending it again could apply it twice:
/// a `DELETE` replayed this way answers 404 for a delete that worked.
#[tokio::test]
async fn a_connection_dropped_after_the_request_arrived_is_not_resent() {
    let (url, seen) = reads_then_closes();
    let client = Client::builder().no_proxy().build().unwrap();
    let url = format!("{url}/r");

    let err = execute_with_retry_policy(
        || async { Ok(client.delete(&url).send().await?) },
        &fast_policy_n(2),
    )
    .await
    .unwrap_err();

    assert_eq!(seen.load(Ordering::SeqCst), 1, "{err:#}");
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
