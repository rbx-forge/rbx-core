//! Roblox's CSRF dance, once.
//!
//! The legacy endpoints — the ones Open Cloud has no equivalent for, reached
//! with a session cookie — answer the first state-changing request with `403`
//! and an `x-csrf-token` header. The request has to be sent again with that
//! token echoed back, and the token is then good for the rest of the session.
//!
//! ## Why this is in `rbx-core`
//!
//! Because it was in four places, and they had already stopped agreeing.
//! `rbx-init` and `rbx-apikey` carried the same function byte for byte;
//! `rbx-meta` carried two more, hand-unrolled, one in `legacy.rs` and one in
//! `experience_releases.rs`. Three of the four counted `204 No Content` as
//! success and the fourth did not, which is the failure mode duplicated
//! protocol logic always produces: nobody edits four copies, so the copies
//! drift, and the drift is in whichever one is read least.
//!
//! What a caller keeps is the part that is genuinely theirs: the message a
//! refusal turns into. That is why this returns [`Refusal`] rather than a
//! formatted error — `rbx-meta` reads the raw body of a failed retry to spot
//! beta-mode experiences, and that body is gone the moment an error is built
//! from it.

use std::sync::Arc;

use reqwest::{RequestBuilder, Response, StatusCode};
use tokio::sync::Mutex;

/// The token cache, shared by every request a client makes.
///
/// A `tokio` mutex rather than a `std` one because it is held across `await`
/// points, and `Arc` because the clients that hold it are cloned into request
/// closures. Cloning shares the cache, which is the point: the token is
/// fetched once per session, not once per call.
#[derive(Debug, Clone, Default)]
pub struct CsrfToken(Arc<Mutex<Option<String>>>);

impl CsrfToken {
    pub fn new() -> Self {
        Self::default()
    }
}

/// A request Roblox answered and refused.
///
/// Carries the raw body rather than a message, so the caller can build the
/// error its own users need: a bare status, a status with context, or a hint
/// read off the body itself.
#[derive(Debug)]
pub struct Refusal {
    pub status: StatusCode,
    pub body: String,
    /// True when this was the second attempt, after a token refresh. Callers
    /// that want to say "retried and failed again" branch on it; the ones that
    /// do not can ignore it.
    pub retried: bool,
}

/// Send a state-changing request, handling the token dance.
///
/// `build` is called once per attempt rather than taking a built request,
/// because a `RequestBuilder` cannot be cloned once a body is attached, and
/// the retry needs a second one.
///
/// Success is `2xx`, `204 No Content` included. That last part is the
/// behaviour three of the four merged implementations had and the fourth did
/// not; unifying on it means an endpoint that answers 204 is not reported as
/// a failure by one command and a success by another.
pub async fn send_with_csrf<F>(token: &CsrfToken, build: F) -> Result<Response, CsrfError>
where
    F: Fn() -> RequestBuilder,
{
    for attempt in 0..2 {
        let cached = token.0.lock().await.clone();
        let mut req = build();
        if let Some(t) = &cached {
            req = req.header("x-csrf-token", t);
        }

        let response = req
            .send()
            .await
            .map_err(|e| CsrfError::Transport(e.into()))?;
        let status = response.status();
        if status.is_success() || status == StatusCode::NO_CONTENT {
            return Ok(response);
        }

        if status == StatusCode::FORBIDDEN && attempt == 0 {
            if let Some(fresh) = response
                .headers()
                .get("x-csrf-token")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
            {
                *token.0.lock().await = Some(fresh);
                continue;
            }
        }

        let body = response.text().await.unwrap_or_default();
        return Err(CsrfError::Refused(Refusal {
            status,
            body,
            retried: attempt > 0,
        }));
    }

    unreachable!("the loop returns on both attempts")
}

/// Why a request did not come back with a response.
#[derive(Debug)]
pub enum CsrfError {
    /// The request never reached Roblox, or its answer never arrived.
    Transport(anyhow::Error),
    /// Roblox answered, and said no.
    Refused(Refusal),
}

impl From<CsrfError> for anyhow::Error {
    /// The default rendering, for callers with nothing to add: a transport
    /// failure as it came, a refusal as the shared Roblox error.
    fn from(e: CsrfError) -> Self {
        match e {
            CsrfError::Transport(err) => err,
            CsrfError::Refused(r) => super::roblox_error(r.status, &r.body),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    /// The dance itself: a 403 carrying a token is answered by one retry that
    /// echoes it, and the caller sees only the success.
    #[tokio::test]
    async fn a_refusal_carrying_a_token_is_retried_once_with_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/thing"))
            .and(header("x-csrf-token", "fresh"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/thing"))
            .respond_with(ResponseTemplate::new(403).insert_header("x-csrf-token", "fresh"))
            .mount(&server)
            .await;

        let url = format!("{}/thing", server.uri());
        let token = CsrfToken::new();
        let response = send_with_csrf(&token, || client().post(&url))
            .await
            .unwrap();

        assert!(response.status().is_success());
        assert_eq!(
            token.0.lock().await.as_deref(),
            Some("fresh"),
            "the token is cached for the rest of the session"
        );
    }

    /// 204 is success. Three of the four implementations this replaces agreed;
    /// the fourth reported it as a failure, which is the divergence that made
    /// merging them worth doing.
    #[tokio::test]
    async fn no_content_is_success() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let url = format!("{}/thing", server.uri());
        let response = send_with_csrf(&CsrfToken::new(), || client().delete(&url))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    /// A 403 with no token to echo is a real refusal, not the dance. Retrying
    /// it would send the same request twice for nothing.
    #[tokio::test]
    async fn a_forbidden_without_a_token_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string("no"))
            .mount(&server)
            .await;

        let url = format!("{}/thing", server.uri());
        let err = send_with_csrf(&CsrfToken::new(), || client().post(&url))
            .await
            .unwrap_err();

        match err {
            CsrfError::Refused(r) => {
                assert_eq!(r.status, StatusCode::FORBIDDEN);
                assert_eq!(r.body, "no");
                assert!(!r.retried);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "one attempt, since there was no token to try again with"
        );
    }

    /// The refusal hands back the raw body, which is what lets `rbx-meta` read
    /// a beta-mode hint off it. An error built here would have eaten it.
    #[tokio::test]
    async fn a_second_refusal_says_it_was_retried_and_keeps_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-csrf-token", "fresh"))
            .respond_with(ResponseTemplate::new(400).set_body_string("still no"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).insert_header("x-csrf-token", "fresh"))
            .mount(&server)
            .await;

        let url = format!("{}/thing", server.uri());
        let err = send_with_csrf(&CsrfToken::new(), || client().post(&url))
            .await
            .unwrap_err();

        match err {
            CsrfError::Refused(r) => {
                assert!(r.retried, "the caller can say it was tried twice");
                assert_eq!(r.body, "still no");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}
