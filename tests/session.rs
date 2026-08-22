//! The cookie check, over HTTP (#63).
//!
//! Every test names its own cookie value. The verdict cache is process-wide by
//! design (one check per execution, however many steps consume the cookie)
//! and `cargo test` runs these in one process, so two tests sharing a cookie
//! string would share a verdict and the "asked exactly once" test would pass
//! or fail depending on the order the others ran in.

use rbx_core::api::build_client;
use rbx_core::session::{self, Session};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const AUTHENTICATED: &str = "/v1/users/authenticated";

/// A mock `users.roblox.com` answering the one call this module makes.
async fn answering(status: u16, body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(AUTHENTICATED))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_live_session_is_valid_and_names_the_account() {
    let server = answering(
        200,
        r#"{"id":156,"name":"builderman","displayName":"Builder Man"}"#,
    )
    .await;

    let verdict = session::check_with_host(&build_client(), "live-session", &server.uri()).await;

    match verdict {
        Session::Valid(account) => {
            assert_eq!(account.id, 156);
            assert_eq!(account.label(), "builderman (156)");
        }
        other => panic!("expected a valid session, got {other:?}"),
    }
    assert!(
        session::require_valid_with_host(&build_client(), "live-session", &server.uri())
            .await
            .is_ok()
    );
}

/// The cookie goes out in the form Roblox expects, whichever form it arrived
/// in. A check that sent a different header from the calls it vouches for
/// would vouch for nothing.
#[tokio::test]
async fn the_cookie_is_sent_with_the_prefix_roblox_expects() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(AUTHENTICATED))
        .and(header("cookie", ".ROBLOSECURITY=raw-value"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":1,"name":"a"}"#))
        .mount(&server)
        .await;

    assert!(matches!(
        session::check_with_host(&build_client(), "raw-value", &server.uri()).await,
        Session::Valid(_)
    ));
    assert!(matches!(
        session::check_with_host(&build_client(), ".ROBLOSECURITY=raw-value", &server.uri()).await,
        Session::Valid(_)
    ));
}

/// The point of the whole module: a refusal is a refusal, and the message says
/// what happened and how to fix it rather than quoting a status code at
/// somebody who cannot act on one.
#[tokio::test]
async fn an_expired_session_is_refused_in_words_a_reader_can_act_on() {
    let server = answering(
        401,
        r#"{"errors":[{"code":0,"message":"Authorization has been denied for this request."}]}"#,
    )
    .await;

    let verdict = session::check_with_host(&build_client(), "expired-session", &server.uri()).await;
    assert_eq!(verdict, Session::Refused);
    assert!(verdict.blocks_a_write());

    let error = session::require_valid_with_host(&build_client(), "expired-session", &server.uri())
        .await
        .expect_err("an expired session must stop the command")
        .to_string();

    assert!(error.contains("expired"), "got {error}");
    assert!(error.contains("Roblox Studio"), "got {error}");
    assert!(error.contains("RBX_COOKIE"), "got {error}");
    assert!(
        !error.contains("401"),
        "a status code is not a remedy: {error}"
    );
}

/// Offline is not expired. Reporting it as one sends somebody to
/// re-authenticate a session that is fine, and hides the actual fault.
#[tokio::test]
async fn an_unreachable_service_is_indeterminate_rather_than_invalid() {
    let unreachable = "http://127.0.0.1:1";

    let verdict = session::check_with_host(&build_client(), "offline-session", unreachable).await;

    assert!(
        matches!(verdict, Session::Unknown(_)),
        "expected an unanswered check, got {verdict:?}"
    );
    assert!(!verdict.blocks_a_write());
    assert!(
        session::require_valid_with_host(&build_client(), "offline-session", unreachable)
            .await
            .is_ok(),
        "a network fault must not be turned into a refusal"
    );
}

/// Same rule for an answer that is not about the session. A 500 or a rate
/// limit is Roblox declining to answer, not Roblox saying no.
#[tokio::test]
async fn a_server_error_is_indeterminate_too() {
    let server = answering(500, r#"{"message":"internal"}"#).await;

    let verdict = session::check_with_host(&build_client(), "500-session", &server.uri()).await;

    match &verdict {
        Session::Unknown(why) => assert!(why.contains("500"), "got {why}"),
        other => panic!("expected an unanswered check, got {other:?}"),
    }
    assert!(
        session::require_valid_with_host(&build_client(), "500-session", &server.uri())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_rate_limit_is_not_a_verdict_on_the_session() {
    let server = answering(429, "Too many requests").await;

    let verdict = session::check_with_host(&build_client(), "429-session", &server.uri()).await;

    assert!(
        matches!(verdict, Session::Unknown(_)),
        "expected an unanswered check, got {verdict:?}"
    );
}

/// One check per execution, however many steps ask. A command that consumes
/// the cookie in three places must not spend three round trips proving the
/// same thing.
#[tokio::test]
async fn the_service_is_asked_exactly_once_per_execution() {
    let server = answering(200, r#"{"id":7,"name":"once"}"#).await;
    let client = build_client();

    for _ in 0..4 {
        session::require_valid_with_host(&client, "cached-session", &server.uri())
            .await
            .expect("a valid session");
    }
    session::check_with_host(&client, "cached-session", &server.uri()).await;

    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "the verdict is reached once and reused"
    );
}

/// A refusal is cached like anything else: five steps into a dead session must
/// not mean five identical refused calls.
#[tokio::test]
async fn a_refusal_is_reached_once_and_reused() {
    let server = answering(401, "{}").await;
    let client = build_client();

    for _ in 0..3 {
        assert!(
            session::require_valid_with_host(&client, "cached-refusal", &server.uri())
                .await
                .is_err()
        );
    }

    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// An empty cookie is a configuration mistake with its own remedy, answered
/// without asking anybody. `RBX_COOKIE=` is how one deliberately says "no
/// cookie", so reporting it as an expired session would be wrong twice over.
#[tokio::test]
async fn an_empty_cookie_is_not_an_expired_one() {
    let server = answering(200, r#"{"id":1,"name":"a"}"#).await;

    let verdict = session::check_with_host(&build_client(), "", &server.uri()).await;

    assert_eq!(verdict, Session::Empty);
    assert!(verdict.blocks_a_write());
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "there is nothing to ask about"
    );
}
