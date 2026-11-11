//! Which redirects the shared clients follow, and how a request that carries
//! no credential follows the rest.
//!
//! reqwest's default policy removes `Authorization` and `Cookie` when a
//! redirect changes host, and forwards every other header, `x-api-key`
//! included. An Open Cloud endpoint that redirected to another host would hand
//! it the key. A policy cannot edit the headers of the next hop, only follow,
//! stop or fail, so the shared clients stop at the first hop that leaves the
//! origin: nothing the caller attached reaches another origin, whatever the
//! header is called.
//!
//! A download from a CDN is the request that needs those hops, and it carries
//! no credential. [`get_without_credentials`] follows them itself.

use anyhow::{bail, Context, Result};
use reqwest::redirect::Policy;
use reqwest::{header, Client, Response, Url};

/// As many redirects as one request follows: reqwest's own default.
const MAX_REDIRECTS: usize = 10;

/// Follow while the request stays on its origin (scheme, host and port), stop
/// at the first hop that leaves it.
///
/// Stopping hands the 3xx to the caller as the response, rather than failing:
/// the retry layer reports it with its status, and a caller that wants the
/// next hop can read its `Location`.
pub(super) fn same_origin() -> Policy {
    Policy::custom(|attempt| {
        let leaves_origin = attempt
            .previous()
            .first()
            .is_none_or(|first| first.origin() != attempt.url().origin());
        if leaves_origin {
            attempt.stop()
        } else if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else {
            attempt.follow()
        }
    })
}

/// GET `url` with no credential, following every redirect, to another origin
/// included.
///
/// For a download from the address an API answered with: a CDN, whose URL
/// carries its own signature. The request has nothing on it to leak, so a hop
/// to another host is safe to follow, and a client from
/// [`build_client`](super::build_client) would stop there instead.
///
/// The response that ends the chain comes back whatever its status, and
/// checking it is the caller's. No error names the URL, the first or any hop
/// after it, since a signed URL in an error is a working download link in a
/// log.
pub async fn get_without_credentials(client: &Client, url: &str) -> Result<Response> {
    let mut url = Url::parse(url).context("the address to download from is not a URL")?;
    for _ in 0..=MAX_REDIRECTS {
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        let Some(location) = response.headers().get(header::LOCATION) else {
            return Ok(response);
        };
        url = location
            .to_str()
            .ok()
            .and_then(|location| url.join(location).ok())
            .with_context(|| {
                format!(
                    "{} redirected to an unreadable location",
                    url.host_str().unwrap_or("the server")
                )
            })?;
    }
    bail!("more than {MAX_REDIRECTS} redirects")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::api::{api_status, build_client, build_client_with_user_agent, execute_with_retry};
    use reqwest::StatusCode;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn redirect_to(location: &str) -> ResponseTemplate {
        ResponseTemplate::new(302).insert_header("location", location)
    }

    fn both_clients() -> [Client; 2] {
        [build_client(), build_client_with_user_agent("test-agent")]
    }

    /// Two mock servers listen on two ports, which are two origins.
    async fn redirect_elsewhere() -> (MockServer, MockServer) {
        let api = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/moved"))
            .respond_with(redirect_to(&format!("{}/landing", elsewhere.uri())))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/landing"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&elsewhere)
            .await;
        (api, elsewhere)
    }

    /// The key stays on the origin it was sent to. reqwest's default policy
    /// forwards it, since it drops only `Authorization` and `Cookie`.
    #[tokio::test]
    async fn a_redirect_to_another_origin_does_not_carry_the_key() {
        for client in both_clients() {
            let (api, elsewhere) = redirect_elsewhere().await;

            let response = client
                .get(format!("{}/moved", api.uri()))
                .header("x-api-key", "the-key")
                .send()
                .await
                .unwrap();

            let reached = elsewhere.received_requests().await.unwrap();
            assert!(
                reached.iter().all(|r| r.headers.get("x-api-key").is_none()),
                "the key must not reach another origin"
            );
            assert_eq!(
                response.status(),
                StatusCode::FOUND,
                "the caller sees the redirect it was not taken through"
            );
        }
    }

    /// Through the retry layer, the stop is an answer with its status, which a
    /// caller can branch on like any refusal.
    #[tokio::test]
    async fn the_retry_layer_reports_a_stopped_redirect_by_its_status() {
        let (api, _elsewhere) = redirect_elsewhere().await;
        let client = build_client();
        let url = format!("{}/moved", api.uri());

        let error = execute_with_retry(|| async {
            Ok(client.get(&url).header("x-api-key", "k").send().await?)
        })
        .await
        .unwrap_err();

        assert_eq!(api_status(&error), Some(StatusCode::FOUND));
    }

    /// A redirect on the same origin is followed, key and all, so a moved
    /// endpoint keeps working.
    #[tokio::test]
    async fn a_redirect_on_the_same_origin_keeps_the_key() {
        for client in both_clients() {
            let api = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/old"))
                .respond_with(redirect_to("/new"))
                .mount(&api)
                .await;
            Mock::given(method("GET"))
                .and(path("/new"))
                .and(header("x-api-key", "the-key"))
                .respond_with(ResponseTemplate::new(200).set_body_string("here"))
                .mount(&api)
                .await;

            let response = client
                .get(format!("{}/old", api.uri()))
                .header("x-api-key", "the-key")
                .send()
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.text().await.unwrap(), "here");
        }
    }

    #[tokio::test]
    async fn a_redirect_loop_on_the_same_origin_ends_in_an_error() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/loop"))
            .respond_with(redirect_to("/loop"))
            .mount(&api)
            .await;

        let error = build_client()
            .get(format!("{}/loop", api.uri()))
            .send()
            .await
            .unwrap_err();

        assert!(error.is_redirect(), "got {error}");
    }

    /// The CDN hop: no credential on it, so a hop to another origin is
    /// followed, which the shared client alone would stop at.
    #[tokio::test]
    async fn a_request_without_credentials_follows_a_redirect_to_another_origin() {
        let (api, elsewhere) = redirect_elsewhere().await;

        let response = get_without_credentials(&build_client(), &format!("{}/moved", api.uri()))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(elsewhere.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_redirect_loop_across_origins_ends_in_an_error() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(redirect_to(&format!("{}/b", b.uri())))
            .mount(&a)
            .await;
        Mock::given(method("GET"))
            .respond_with(redirect_to(&format!("{}/a", a.uri())))
            .mount(&b)
            .await;

        let error = get_without_credentials(&build_client(), &format!("{}/a", a.uri()))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("redirects"), "got {error}");
    }

    /// A signed URL is a working download link, so a failure does not print
    /// it.
    #[tokio::test]
    async fn a_failure_does_not_print_the_url() {
        let error = get_without_credentials(
            &build_client(),
            "http://127.0.0.1:1/blob?signature=aaaaBBBBccccDDDD",
        )
        .await
        .unwrap_err();

        let shown = format!("{error:#}");
        assert!(!shown.contains("aaaaBBBBccccDDDD"), "got {shown}");
        assert!(!shown.contains("/blob"), "got {shown}");
    }
}
