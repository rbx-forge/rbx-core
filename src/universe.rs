//! Listing a universe's places, without a project on disk.
//!
//! Open Cloud cannot enumerate places: `/cloud/v2/universes/{id}/places/{p}`
//! reads one place but only if you already know its id. The enumeration lives
//! on the legacy `develop` host, which is why this module exists at all.
//!
//! **No credential is required.** Measured against a private universe with no
//! key and no cookie, this listing answers 200 with every place and its name;
//! visibility does not gate it. The `cookie` parameter is threaded through
//! because callers have one and sending it costs nothing, not because a
//! private universe needs it. See `docs/cookie.md`, "What the cookie does not
//! protect".
//!
//! Shared here rather than in a domain crate because four crates had grown
//! their own copy of the same two calls (`rbx-init`, `rbx-import`, `rbx-place`,
//! and now `rbx-open`), each with its own pagination loop to get wrong.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::api::{encode_query_value, execute_json, ApiBase};

/// The legacy host that answers both calls in this module.
pub const DEVELOP_HOST: &str = "https://develop.roblox.com";

/// One place in a universe, as Roblox reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniversePlace {
    pub id: u64,
    /// What Roblox calls it. Can be empty: the field is absent on some places,
    /// and callers that display it have to cope rather than assume.
    pub name: String,
    /// The universe's start place. Exactly one entry has this set.
    pub is_root: bool,
}

#[derive(Debug, Deserialize)]
struct DevelopUniverse {
    #[serde(rename = "rootPlaceId", default)]
    root_place_id: u64,
}

#[derive(Debug, Deserialize)]
struct DevelopPlace {
    #[serde(default, alias = "placeId")]
    id: u64,
    #[serde(default, alias = "displayName")]
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevelopPlacesPage {
    data: Vec<DevelopPlace>,
    next_page_cursor: Option<String>,
}

/// The id of the universe's start place.
pub async fn root_place_id(
    client: &reqwest::Client,
    develop: &ApiBase,
    cookie: Option<&str>,
    universe_id: u64,
) -> Result<u64> {
    let url = develop.join(&format!("/v1/universes/{universe_id}"));
    let info: DevelopUniverse = execute_json(|| async {
        let mut req = client.get(&url);
        if let Some(c) = cookie {
            req = req.header(reqwest::header::COOKIE, c);
        }
        Ok(req.send().await?)
    })
    .await
    .with_context(|| format!("Failed to resolve the root place of universe {universe_id}"))?;

    if info.root_place_id == 0 {
        // Deliberately does not suggest --cookie. This endpoint answers 200 on
        // a private universe with no credential at all, so a cookie cannot be
        // the missing piece, and suggesting it sends the reader off to fix the
        // one thing that was never wrong. A bad id is what is left.
        bail!(
            "Could not resolve the root place of universe {universe_id}. Check the id: this \
             listing answers without any credential, so a missing cookie is not the cause. \
             A place id passed where a universe id belongs is the usual mistake."
        );
    }
    Ok(info.root_place_id)
}

/// Every place in the universe, root first.
///
/// The root is seeded from [`root_place_id`] rather than searched for in the
/// listing: for some universes the start place does not appear in the listing
/// at all, and a caller that took `data[0]` would open the wrong place.
pub async fn list_places(
    client: &reqwest::Client,
    develop: &ApiBase,
    cookie: Option<&str>,
    universe_id: u64,
) -> Result<Vec<UniversePlace>> {
    let root_id = root_place_id(client, develop, cookie, universe_id).await?;

    let mut raw: Vec<DevelopPlace> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut url = develop.join(&format!(
            "/v1/universes/{universe_id}/places?isUniverseCreation=false&limit=100&sortOrder=Asc"
        ));
        if let Some(c) = &cursor {
            // Encoded, not pasted: the cursor is an opaque token and a `+` or
            // `&` in it would silently re-request page one for ever.
            url.push_str("&cursor=");
            url.push_str(&encode_query_value(c));
        }
        let page: DevelopPlacesPage = execute_json(|| async {
            let mut req = client.get(&url);
            if let Some(c) = cookie {
                req = req.header(reqwest::header::COOKIE, c);
            }
            Ok(req.send().await?)
        })
        .await
        .with_context(|| format!("Failed to list places for universe {universe_id}"))?;

        raw.extend(page.data);
        match page.next_page_cursor {
            Some(c) if !c.is_empty() => cursor = Some(c),
            _ => break,
        }
    }

    let root_name = raw
        .iter()
        .find(|p| p.id == root_id)
        .map(|p| p.name.clone())
        .unwrap_or_default();

    let mut places = vec![UniversePlace {
        id: root_id,
        name: root_name,
        is_root: true,
    }];
    for place in raw {
        if place.id == root_id {
            continue;
        }
        places.push(UniversePlace {
            id: place.id,
            name: place.name,
            is_root: false,
        });
    }

    Ok(places)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const UNIVERSE: u64 = 9876543210;
    const ROOT: u64 = 111111111;

    fn base(server: &MockServer) -> ApiBase {
        ApiBase::new(server.uri())
    }

    async fn mount_root(server: &MockServer, root_place_id: u64) {
        Mock::given(method("GET"))
            .and(path(format!("/v1/universes/{UNIVERSE}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "rootPlaceId": root_place_id })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn the_root_place_comes_first_even_when_roblox_lists_it_last() {
        let server = MockServer::start().await;
        mount_root(&server, ROOT).await;
        Mock::given(method("GET"))
            .and(path(format!("/v1/universes/{UNIVERSE}/places")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "id": 222, "name": "Lobby" },
                    { "id": ROOT, "name": "Start" },
                ],
                "nextPageCursor": null
            })))
            .mount(&server)
            .await;

        let places = list_places(&reqwest::Client::new(), &base(&server), None, UNIVERSE)
            .await
            .expect("listing should succeed");

        assert_eq!(places[0].id, ROOT);
        assert_eq!(places[0].name, "Start");
        assert!(places[0].is_root);
        assert_eq!(places[1].id, 222);
        assert!(!places[1].is_root);
    }

    /// The case that makes seeding the root necessary rather than tidy: some
    /// universes do not return the start place in their own place listing.
    #[tokio::test]
    async fn the_root_place_is_present_even_when_the_listing_omits_it() {
        let server = MockServer::start().await;
        mount_root(&server, ROOT).await;
        Mock::given(method("GET"))
            .and(path(format!("/v1/universes/{UNIVERSE}/places")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "id": 222, "name": "Lobby" }],
                "nextPageCursor": null
            })))
            .mount(&server)
            .await;

        let places = list_places(&reqwest::Client::new(), &base(&server), None, UNIVERSE)
            .await
            .expect("listing should succeed");

        assert_eq!(places.len(), 2);
        assert_eq!(places[0].id, ROOT);
        assert!(places[0].is_root);
        // No name to be had; the caller has to render an id-only entry.
        assert_eq!(places[0].name, "");
    }

    /// A universe with more than 100 places is the only way the cursor path
    /// runs, and getting it wrong loops on page one for ever.
    #[tokio::test]
    async fn pagination_follows_the_cursor_and_stops() {
        let server = MockServer::start().await;
        mount_root(&server, ROOT).await;
        let list = format!("/v1/universes/{UNIVERSE}/places");

        Mock::given(method("GET"))
            .and(path(list.clone()))
            .and(query_param_is_missing("cursor"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "id": ROOT, "name": "Start" }],
                "nextPageCursor": "page+two"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(list))
            // The `+` must arrive as a `+`, not as the space it decodes to.
            .and(query_param("cursor", "page+two"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "id": 222, "name": "Lobby" }],
                "nextPageCursor": ""
            })))
            .mount(&server)
            .await;

        let places = list_places(&reqwest::Client::new(), &base(&server), None, UNIVERSE)
            .await
            .expect("listing should succeed");

        assert_eq!(places.len(), 2);
        assert_eq!(places[1].id, 222);
    }

    /// `placeId`/`displayName` is what the endpoint actually sends on some
    /// routes; the aliases are load-bearing, not decoration.
    #[tokio::test]
    async fn the_camel_case_aliases_are_accepted() {
        let server = MockServer::start().await;
        mount_root(&server, ROOT).await;
        Mock::given(method("GET"))
            .and(path(format!("/v1/universes/{UNIVERSE}/places")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "placeId": 222, "displayName": "Lobby" }],
                "nextPageCursor": null
            })))
            .mount(&server)
            .await;

        let places = list_places(&reqwest::Client::new(), &base(&server), None, UNIVERSE)
            .await
            .expect("listing should succeed");

        assert_eq!(places[1].id, 222);
        assert_eq!(places[1].name, "Lobby");
    }

    /// A place id passed where a universe id belongs is the common typo, and
    /// it comes back as `rootPlaceId: 0` rather than as an error status.
    #[tokio::test]
    async fn a_zero_root_place_id_is_an_error_that_does_not_blame_the_cookie() {
        let server = MockServer::start().await;
        mount_root(&server, 0).await;

        let error = list_places(&reqwest::Client::new(), &base(&server), None, UNIVERSE)
            .await
            .expect_err("a zero root place id must not be reported as success");

        let text = format!("{error:#}");
        assert!(text.contains("Check the id"), "got: {text}");
        assert!(
            !text.contains("--cookie"),
            "must not send the reader after a credential that changes nothing: {text}"
        );
    }
}
