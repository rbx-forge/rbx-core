//! Asset download via the Open Cloud asset delivery API. Returns raw bytes
//! so callers can hash them, save them to disk, or transform them as needed.

use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;

use super::base::ApiBase;
use super::retry::execute_json;

#[derive(Debug, Deserialize)]
struct AssetDeliveryResponse {
    location: String,
}

/// Fetch the bytes of a Roblox asset given its asset id. Goes through the
/// asset delivery API to resolve the CDN URL, then downloads from it.
pub async fn download_asset(client: &Client, api_key: &str, asset_id: u64) -> Result<Vec<u8>> {
    download_asset_from(client, api_key, asset_id, &ApiBase::default()).await
}

/// [`download_asset`] against a named host.
///
/// Only the *first* of the two requests is redirected. The second goes to
/// whatever URL the delivery API answered with, which is a CDN host this code
/// never chooses — pointing it somewhere would be inventing behaviour rather
/// than exercising it. A test therefore answers with its own address and gets
/// both halves on one server.
pub async fn download_asset_from(
    client: &Client,
    api_key: &str,
    asset_id: u64,
    base: &ApiBase,
) -> Result<Vec<u8>> {
    let url = base.join(&format!("/asset-delivery-api/v1/assetId/{asset_id}"));
    let resp: AssetDeliveryResponse =
        execute_json(|| async { Ok(client.get(&url).header("x-api-key", api_key).send().await?) })
            .await?;

    let bytes = client.get(&resp.location).send().await?.bytes().await?;
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

        let bytes =
            download_asset_from(&Client::new(), "test-key", 123, &ApiBase::new(server.uri()))
                .await
                .unwrap();

        assert_eq!(bytes, b"PNG-ish");
    }

    /// The api key is only on the first request. The CDN URL carries its own
    /// authorisation in the query string, and sending a key to a third-party
    /// host would leak it.
    #[tokio::test]
    async fn the_api_key_is_not_sent_to_the_cdn() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/asset-delivery-api/v1/assetId/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "location": format!("{}/cdn/x", server.uri())
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/cdn/x"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&server)
            .await;

        download_asset_from(&Client::new(), "secret", 7, &ApiBase::new(server.uri()))
            .await
            .unwrap();

        let cdn = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.url.path() == "/cdn/x")
            .unwrap();
        assert!(
            cdn.headers.get("x-api-key").is_none(),
            "the key must not reach the CDN"
        );
    }

    #[tokio::test]
    async fn a_refused_delivery_lookup_is_an_error_rather_than_empty_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/asset-delivery-api/v1/assetId/9"))
            .respond_with(ResponseTemplate::new(403).set_body_string("no"))
            .mount(&server)
            .await;

        assert!(
            download_asset_from(&Client::new(), "k", 9, &ApiBase::new(server.uri()))
                .await
                .is_err()
        );
    }
}
