//! Open Cloud HTTP client with retry and asset download. Domain crates
//! (`rbx-shop`, `rbx-meta`, etc.) wrap this for their endpoint-specific
//! calls.

mod asset;
mod base;
mod csrf;
mod error;
mod retry;

pub use asset::{download_asset, download_asset_from};
pub use base::{encode_query_value, explain_missing_scope, ApiBase, DEFAULT_API_BASE};
pub use csrf::{send_with_csrf, CsrfError, CsrfToken, Refusal};
pub use error::{api_status, is_api_status, roblox_error, roblox_message, ApiError};
pub use retry::{execute_json, execute_with_retry, execute_with_retry_policy, RetryPolicy};

use std::time::Duration;

use anyhow::Result;
use reqwest::Client;

/// Default per-request timeout. Open Cloud calls that take longer than this
/// almost always indicate something is wrong (a hung connection, a Roblox
/// outage). One minute is generous enough for legitimate slow responses
/// (large asset uploads, list endpoints with thousands of items) without
/// letting CI pipelines hang indefinitely.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Build the shared `reqwest::Client` used across the suite. `gzip` is
/// enabled because Open Cloud responses are often large JSON. Timeout caps
/// any single request at [`DEFAULT_REQUEST_TIMEOUT`] so a hung connection
/// can't block the binary forever.
pub fn build_client() -> Client {
    Client::builder()
        .gzip(true)
        .timeout(DEFAULT_REQUEST_TIMEOUT)
        .build()
        .expect("reqwest::Client::builder should not fail on standard config")
}

/// Like [`build_client`] but with a fixed `User-Agent`. Crates that hit the
/// legacy/web Roblox endpoints (rbx-apikey, rbx-init) set one because some of
/// those endpoints reject the default reqwest agent. Falls back to a bare
/// client if the builder ever fails, so a UA quirk can't take the tool down.
pub fn build_client_with_user_agent(user_agent: &str) -> Client {
    Client::builder()
        .user_agent(user_agent)
        .gzip(true)
        .timeout(DEFAULT_REQUEST_TIMEOUT)
        .build()
        .unwrap_or_else(|_| Client::new())
}

/// Resolve the `x-api-key` header value from a `GlobalFlags`-style optional
/// key. Returns an actionable error when missing.
pub fn require_api_key(api_key: Option<&str>) -> Result<&str> {
    api_key.ok_or_else(|| {
        anyhow::anyhow!("Roblox Open Cloud API key required. Pass --api-key or set RBX_API_KEY.")
    })
}
