//! Asset download via the Open Cloud asset delivery API. Returns raw bytes
//! so callers can hash them, save them to disk, or transform them as needed.

use std::fmt;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use super::base::ApiBase;
use super::redirect::{follow, no_redirect_client};
use super::retry::{execute_with_retry, status_text};

#[derive(Deserialize)]
struct AssetDeliveryResponse {
    location: String,
}

/// Hand-written rather than derived: `location` is a signed URL, a working
/// download link in whatever log a `{:?}` reaches.
impl fmt::Debug for AssetDeliveryResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { location: _ } = self;
        f.debug_struct("AssetDeliveryResponse")
            .field("location", &"<redacted>")
            .finish()
    }
}

/// Fetch the bytes of a Roblox asset given its asset id. Goes through the
/// asset delivery API to resolve the CDN URL, then downloads from it.
pub async fn download_asset(api_key: &str, asset_id: u64) -> Result<Vec<u8>> {
    download_asset_from(api_key, asset_id, &ApiBase::default()).await
}

/// [`download_asset`] against a named host.
///
/// Only the *first* of the two requests goes to `base`. The second goes to
/// whatever URL the delivery API answered with, which is a CDN host this code
/// never chooses: pointing it somewhere would be inventing behaviour rather
/// than exercising it. A test therefore answers with its own address and gets
/// both halves on one server.
///
/// Both requests go through a client of this function's own that follows no
/// redirect, and their redirects are followed one hop at a time. The key goes
/// on the lookup, and on a redirect of it only while that stays on the
/// lookup's origin: once a hop leaves, the key is dropped for the rest of the
/// chain. The download from the CDN carries no credential at all, and follows
/// its redirects to any host. Where the key goes is therefore this function's
/// to decide, not that of a client handed in.
///
/// Every hop is checked: a lookup refused by Roblox, or a CDN that does not
/// answer with a success, is an error rather than bytes. A CDN failure says
/// its status and nothing else, since its body can echo the signature of the
/// URL, and so does a lookup answered from another origin after a redirect.
/// No error names the URL the lookup answered with.
pub async fn download_asset_from(api_key: &str, asset_id: u64, base: &ApiBase) -> Result<Vec<u8>> {
    let client = no_redirect_client()?;
    let url = base.join(&format!("/asset-delivery-api/v1/assetId/{asset_id}"));
    let answer = execute_with_retry(|| async {
        let lookup = follow(&client, &url, Some(("x-api-key", api_key))).await?;
        let status = lookup.response.status();
        if lookup.answered_elsewhere && !status.is_success() {
            // Not handed to the retry layer, which would quote the body.
            bail!(
                "the asset delivery API redirected asset {asset_id} to another host, which \
                 answered {}",
                status_text(status)
            );
        }
        Ok(lookup.response)
    })
    .await?;
    // Read here rather than through `execute_json`, whose error for a body
    // it cannot parse quotes the body, and this body holds a signed URL.
    let body = answer.bytes().await.map_err(reqwest::Error::without_url)?;
    let delivery: AssetDeliveryResponse = serde_json::from_slice(&body).map_err(|e| {
        anyhow!("the delivery API answered asset {asset_id} without a location to download: {e}")
    })?;

    let cdn = follow(&client, &delivery.location, None)
        .await
        .with_context(|| format!("downloading asset {asset_id} from the CDN"))?
        .response;
    let status = cdn.status();
    if !status.is_success() {
        bail!(
            "downloading asset {asset_id} failed: the CDN answered {}",
            status_text(status)
        );
    }
    let bytes = cdn
        .bytes()
        .await
        .map_err(reqwest::Error::without_url)
        .with_context(|| format!("downloading asset {asset_id} from the CDN"))?;
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// What a signed CDN URL carries, and what no error may print.
    const SIGNATURE: &str = "signature=aaaaBBBBccccDDDD";

    async fn delivery_pointing_at(location: String) -> MockServer {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/asset-delivery-api/v1/assetId/7"))
            .and(header("x-api-key", "secret"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "location": location })),
            )
            .mount(&api)
            .await;
        api
    }

    async fn download(api: &MockServer) -> Result<Vec<u8>> {
        download_asset_from("secret", 7, &ApiBase::new(api.uri())).await
    }

    fn redirect_to(location: String) -> ResponseTemplate {
        ResponseTemplate::new(307).insert_header("location", location)
    }

    /// A lookup that moved on its own origin is followed with the key.
    #[tokio::test]
    async fn a_lookup_redirected_on_its_origin_keeps_the_key() {
        let cdn = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"bytes".to_vec()))
            .mount(&cdn)
            .await;
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/asset-delivery-api/v1/assetId/7"))
            .respond_with(redirect_to("/moved".into()))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/moved"))
            .and(header("x-api-key", "secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "location": format!("{}/blob", cdn.uri())
            })))
            .mount(&api)
            .await;

        assert_eq!(download(&api).await.unwrap(), b"bytes");
    }

    /// A lookup redirected to another origin gets there without the key,
    /// whatever client the caller uses elsewhere: the function follows the
    /// hop itself.
    #[tokio::test]
    async fn a_lookup_redirected_to_another_origin_does_not_carry_the_key() {
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "location": format!("{}/blob", elsewhere.uri())
            })))
            .mount(&elsewhere)
            .await;
        Mock::given(method("GET"))
            .and(path("/blob"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"bytes".to_vec()))
            .mount(&elsewhere)
            .await;
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(redirect_to(format!("{}/lookup", elsewhere.uri())))
            .mount(&api)
            .await;

        assert_eq!(download(&api).await.unwrap(), b"bytes");

        let reached = elsewhere.received_requests().await.unwrap();
        assert_eq!(reached.len(), 2);
        assert!(
            reached.iter().all(|r| r.headers.get("x-api-key").is_none()),
            "the key must not follow the lookup to another origin"
        );
    }

    /// A refusal from where the lookup was redirected is that host's, and
    /// its body is not shown.
    #[tokio::test]
    async fn a_refusal_after_the_lookup_left_its_origin_shows_no_body() {
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_string("body-eeeeFFFF"))
            .mount(&elsewhere)
            .await;
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(redirect_to(format!("{}/x?{SIGNATURE}", elsewhere.uri())))
            .mount(&api)
            .await;

        let shown = format!("{:#}", download(&api).await.unwrap_err());

        assert!(shown.contains("403 Forbidden"), "got {shown}");
        assert!(!shown.contains("eeeeFFFF"), "got {shown}");
        assert!(!shown.contains("aaaaBBBBccccDDDD"), "got {shown}");
    }

    /// The two-step this function exists for: the delivery API answers with a
    /// `location`, and the bytes come from there rather than from the call you
    /// made. A caller that returned the first response's body would get JSON
    /// where it expected an image.
    #[tokio::test]
    async fn the_bytes_come_from_the_location_the_delivery_api_names() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/asset-delivery-api/v1/assetId/123"))
            .and(header("x-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "location": format!("{}/cdn/the-file", server.uri())
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/cdn/the-file"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"PNG-ish".to_vec()))
            .mount(&server)
            .await;

        let bytes = download_asset_from("test-key", 123, &ApiBase::new(server.uri()))
            .await
            .unwrap();

        assert_eq!(bytes, b"PNG-ish");
    }

    /// The api key is only on the first request. The CDN URL carries its own
    /// authorisation in the query string, and sending a key to a third-party
    /// host would leak it.
    #[tokio::test]
    async fn the_api_key_is_not_sent_to_the_cdn() {
        let cdn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cdn/x"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&cdn)
            .await;
        let api = delivery_pointing_at(format!("{}/cdn/x?{SIGNATURE}", cdn.uri())).await;

        assert_eq!(download(&api).await.unwrap(), vec![1, 2, 3]);

        let reached = cdn.received_requests().await.unwrap();
        assert_eq!(reached.len(), 1);
        assert!(
            reached[0].headers.get("x-api-key").is_none(),
            "the key must not reach the CDN"
        );
    }

    /// A CDN that redirects to another host is followed there, without the
    /// key.
    #[tokio::test]
    async fn a_cdn_redirect_to_another_host_is_followed_without_the_key() {
        let mirror = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/blob"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"bytes".to_vec()))
            .mount(&mirror)
            .await;
        let cdn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/blob"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/blob?{SIGNATURE}", mirror.uri())),
            )
            .mount(&cdn)
            .await;
        let api = delivery_pointing_at(format!("{}/blob?{SIGNATURE}", cdn.uri())).await;

        assert_eq!(download(&api).await.unwrap(), b"bytes");

        let reached = mirror.received_requests().await.unwrap();
        assert_eq!(reached.len(), 1);
        assert!(reached[0].headers.get("x-api-key").is_none());
    }

    /// A CDN refusal is an error, not an asset. Returning its body as the
    /// bytes is how an XML error page gets saved as an image and hashed into
    /// a lockfile. The error gives the status alone: an S3-style refusal
    /// echoes the signature and the string it signed.
    #[tokio::test]
    async fn a_cdn_refusal_is_an_error_that_shows_neither_its_body_nor_the_url() {
        let cdn = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("<Error>SignatureDoesNotMatch body-eeeeFFFF</Error>"),
            )
            .mount(&cdn)
            .await;
        let api = delivery_pointing_at(format!("{}/blob?{SIGNATURE}", cdn.uri())).await;

        let error = download(&api).await.unwrap_err();

        let shown = format!("{error:#}");
        assert!(shown.contains("403 Forbidden"), "got {shown}");
        assert!(!shown.contains("eeeeFFFF"), "got {shown}");
        assert!(!shown.contains("aaaaBBBBccccDDDD"), "got {shown}");
    }

    /// A delivery answer in a shape this does not read is an error that does
    /// not quote it, since the body is where the signed URL would be.
    #[tokio::test]
    async fn a_delivery_answer_without_a_location_is_not_quoted() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "locations": [{ "location": format!("https://cdn.test/blob?{SIGNATURE}") }]
            })))
            .mount(&api)
            .await;

        let error = download(&api).await.unwrap_err();

        let shown = format!("{error:#}");
        assert!(shown.contains("without a location"), "got {shown}");
        assert!(!shown.contains("aaaaBBBBccccDDDD"), "got {shown}");
    }

    #[test]
    fn debug_does_not_show_the_signed_location() {
        let answer = AssetDeliveryResponse {
            location: format!("https://cdn.test/blob?{SIGNATURE}"),
        };
        let shown = format!("{answer:?} {answer:#?}");
        assert!(!shown.contains("aaaaBBBBccccDDDD"), "got {shown}");
        assert!(!shown.contains("cdn.test"), "got {shown}");
    }

    #[tokio::test]
    async fn a_refused_delivery_lookup_is_an_error_rather_than_empty_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/asset-delivery-api/v1/assetId/9"))
            .respond_with(ResponseTemplate::new(403).set_body_string("no"))
            .mount(&server)
            .await;

        assert!(download_asset_from("k", 9, &ApiBase::new(server.uri()))
            .await
            .is_err());
    }
}
