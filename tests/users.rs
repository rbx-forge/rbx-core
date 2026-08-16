//! User resolution over HTTP, against a mock of the public `users.roblox.com`
//! endpoints. The response shapes are copied from real calls.

use rbx_core::api::build_client;
use rbx_core::users::{resolve_with_host, UserRef};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The real endpoint answers a batch and simply leaves out anything it did not
/// find. Confirmed against the live API: asking for three names where one is
/// nonsense returns two rows and no error.
async fn mount_name_lookup(server: &MockServer, body: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/v1/usernames/users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_username_resolves_to_its_id() {
    let server = MockServer::start().await;
    mount_name_lookup(
        &server,
        serde_json::json!({"data":[{
            "requestedUsername":"builderman","id":156,
            "name":"builderman","displayName":"builderman","hasVerifiedBadge":true
        }]}),
    )
    .await;

    let users = resolve_with_host(
        &build_client(),
        &[UserRef::parse("builderman").unwrap()],
        &server.uri(),
    )
    .await
    .unwrap();

    assert_eq!(users[0].id, 156);
    assert_eq!(users[0].name, "builderman");
    assert!(users[0].has_verified_badge);
}

#[tokio::test]
async fn a_username_that_does_not_exist_is_an_error_naming_it() {
    // The single most important behaviour here. The endpoint omits unknown
    // names rather than reporting them, so a typo would otherwise resolve to
    // an empty set and the caller would act on nobody, or worse, on a shorter
    // list than it believed.
    let server = MockServer::start().await;
    mount_name_lookup(
        &server,
        serde_json::json!({"data":[{
            "requestedUsername":"builderman","id":156,
            "name":"builderman","displayName":"builderman","hasVerifiedBadge":false
        }]}),
    )
    .await;

    let error = resolve_with_host(
        &build_client(),
        &[
            UserRef::parse("builderman").unwrap(),
            UserRef::parse("ThisNameDoesNotExist99999x").unwrap(),
        ],
        &server.uri(),
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(error.contains("ThisNameDoesNotExist99999x"), "got: {error}");
}

#[tokio::test]
async fn an_id_is_fetched_from_the_by_id_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/users/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":1,"name":"Roblox","displayName":"Roblox","hasVerifiedBadge":true,"isBanned":false
        })))
        .expect(1)
        .mount(&server)
        .await;

    let users = resolve_with_host(
        &build_client(),
        &[UserRef::parse("1").unwrap()],
        &server.uri(),
    )
    .await
    .unwrap();

    assert_eq!(users[0].name, "Roblox");
}

#[tokio::test]
async fn results_come_back_in_the_order_asked_for_not_the_order_answered() {
    // The batch endpoint does not promise to preserve order, and a caller that
    // zips its input against the response would then act on the wrong person.
    let server = MockServer::start().await;
    mount_name_lookup(
        &server,
        serde_json::json!({"data":[
            {"requestedUsername":"bravo","id":2,"name":"bravo","displayName":"bravo"},
            {"requestedUsername":"alpha","id":1,"name":"alpha","displayName":"alpha"}
        ]}),
    )
    .await;

    let users = resolve_with_host(
        &build_client(),
        &[
            UserRef::parse("alpha").unwrap(),
            UserRef::parse("bravo").unwrap(),
        ],
        &server.uri(),
    )
    .await
    .unwrap();

    assert_eq!(users[0].name, "alpha");
    assert_eq!(users[1].name, "bravo");
}

#[tokio::test]
async fn only_one_request_is_made_for_many_names() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/usernames/users"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[
                {"requestedUsername":"a","id":1,"name":"a","displayName":"a"},
                {"requestedUsername":"b","id":2,"name":"b","displayName":"b"},
                {"requestedUsername":"c","id":3,"name":"c","displayName":"c"}
            ]})),
        )
        .expect(1)
        .mount(&server)
        .await;

    resolve_with_host(
        &build_client(),
        &[
            UserRef::parse("a").unwrap(),
            UserRef::parse("b").unwrap(),
            UserRef::parse("c").unwrap(),
        ],
        &server.uri(),
    )
    .await
    .unwrap();
}
