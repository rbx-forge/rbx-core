//! Injectable API host.
//!
//! Domain crates used to build URLs from inline string literals
//! (`format!("https://apis.roblox.com/cloud/v2/...")`), which makes them
//! impossible to point at a mock server: the host is baked into the call
//! site. [`ApiBase`] moves the host to a value the caller owns, so a test can
//! hand a client the URI of a `wiremock` server and exercise pagination,
//! error mapping and retry against real HTTP.
//!
//! A per-client field rather than a global or an env var on purpose. `cargo
//! test` runs tests in parallel within a binary, so process-wide state would
//! let two tests with two different mock servers overwrite each other's host
//! and fail intermittently. Flaky tests are worse than no tests here, because
//! they train you to re-run rather than to read the failure.

use std::fmt;

/// The real Open Cloud host. Everything defaults to this; only tests should
/// pass anything else.
pub const DEFAULT_API_BASE: &str = "https://apis.roblox.com";

/// Base URL every Open Cloud request is built from.
///
/// Construct with [`ApiBase::default`] in production code and
/// [`ApiBase::new`] in tests:
///
/// ```
/// # use rbx_core::api::ApiBase;
/// let base = ApiBase::default();
/// assert_eq!(base.join("/cloud/v2/universes/1"), "https://apis.roblox.com/cloud/v2/universes/1");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiBase(String);

impl ApiBase {
    /// Trailing slashes are stripped so that [`join`](Self::join) can assume
    /// exactly one separator. `wiremock`'s `server.uri()` has none, a host
    /// typed by hand often does, and both must produce the same URL.
    pub fn new(base: impl Into<String>) -> Self {
        let base = base.into();
        Self(base.trim_end_matches('/').to_string())
    }

    /// Append a path. `path` is expected to start with `/`; one is inserted
    /// if it does not, so a caller cannot accidentally produce
    /// `https://apis.roblox.comcloud/v2`.
    pub fn join(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("{}{}", self.0, path)
        } else {
            format!("{}/{}", self.0, path)
        }
    }

    /// The host itself, without a trailing slash.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ApiBase {
    fn default() -> Self {
        Self::new(DEFAULT_API_BASE)
    }
}

impl fmt::Display for ApiBase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Add the missing half of a "scope is missing" error.
///
/// Roblox says which scope was needed and says nothing about which key it
/// checked. `RBX_API_KEY` is one variable name shared by every tool in this
/// suite and by anything else the user has set up, so the usual cause of this
/// error is not a badly declared key but the wrong key being in the
/// environment, which the message gives no way to notice.
///
/// Two things are checked, not one. The wording is Roblox's way of saying the
/// key is underpowered, but the same words can appear in the body of a failure
/// that has nothing to do with scopes: a 500 echoing an upstream message, for
/// one. Advising somebody to re-issue their key because the server broke sends
/// them off fixing what is not wrong, so the hint is withheld unless the status
/// could actually mean "refused": a 4xx, or no HTTP status at all (the error
/// came from somewhere this function cannot judge, and the old text-only
/// behaviour is the safer default there).
///
/// Leaves any other error untouched.
pub fn explain_missing_scope(error: anyhow::Error) -> anyhow::Error {
    // The chain, not just the outermost error: `to_string()` on an anyhow
    // error renders only the top frame, so a caller that added `.context()`
    // before calling this would hide the body the match needs.
    let says_scope = error.chain().any(|cause| {
        let text = cause.to_string();
        text.contains("PERMISSION_DENIED") || text.contains("scope")
    });
    if !says_scope {
        return error;
    }
    if let Some(status) = super::api_status(&error) {
        if !status.is_client_error() {
            return error;
        }
    }
    error.context(
        "The key in use does not carry that scope. `RBX_API_KEY` is a single variable \
         shared by every tool, so the usual cause is a key left over from another project \
         rather than a wrongly declared one. Check what is loaded with \
         `rbx apikey introspect <name>`, and reload the one you meant with \
         `export RBX_API_KEY=\"$(rbx apikey resolve <name>)\"`.",
    )
}

/// Percent-encode a value going into a query string.
///
/// Shared because more than one crate pastes an opaque pagination token into a
/// URL. The tokens are base64url blobs today and have not been seen to contain
/// a reserved character, but they are opaque and Roblox is free to change what
/// goes in them, so they are encoded rather than trusted. A token containing a
/// `+` or `&` and pasted raw silently requests the wrong page.
pub fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreserved_characters_survive_encoding_unchanged() {
        assert_eq!(encode_query_value("abcXYZ019-_.~"), "abcXYZ019-_.~");
    }

    #[test]
    fn reserved_characters_are_escaped() {
        // `+` decoding as a space is the classic way a base64 token silently
        // becomes a different token.
        assert_eq!(encode_query_value("a+b/c=d&e"), "a%2Bb%2Fc%3Dd%26e");
    }

    #[test]
    fn default_is_the_real_open_cloud_host() {
        assert_eq!(ApiBase::default().as_str(), "https://apis.roblox.com");
    }

    #[test]
    fn join_produces_one_separator_when_the_path_has_a_leading_slash() {
        let base = ApiBase::new("https://example.test");
        assert_eq!(
            base.join("/cloud/v2/universes/1"),
            "https://example.test/cloud/v2/universes/1"
        );
    }

    #[test]
    fn join_inserts_the_separator_when_the_path_lacks_one() {
        let base = ApiBase::new("https://example.test");
        assert_eq!(base.join("cloud/v2"), "https://example.test/cloud/v2");
    }

    #[test]
    fn a_trailing_slash_on_the_host_does_not_double_up() {
        let base = ApiBase::new("https://example.test/");
        assert_eq!(base.join("/cloud/v2"), "https://example.test/cloud/v2");
    }

    #[test]
    fn repeated_trailing_slashes_are_all_stripped() {
        let base = ApiBase::new("https://example.test///");
        assert_eq!(base.as_str(), "https://example.test");
    }

    /// The hint's whole value is that it appears exactly when it applies.
    mod explain_missing_scope {
        use super::super::explain_missing_scope;
        use crate::api::ApiError;
        use reqwest::StatusCode;

        /// Substring of the advice, enough to tell it apart from the original.
        const HINT: &str = "does not carry that scope";

        #[test]
        fn a_forbidden_naming_the_scope_gets_the_hint() {
            let error = anyhow::Error::from(ApiError::new(
                StatusCode::FORBIDDEN,
                r#"{"code":"PERMISSION_DENIED","message":"missing scope"}"#,
            ));
            assert!(explain_missing_scope(error).to_string().contains(HINT));
        }

        #[test]
        fn a_server_error_quoting_the_same_words_does_not() {
            // Roblox's 500s echo upstream text. Telling somebody their key is
            // wrong when Roblox is down costs them a key rotation and gets
            // them nowhere.
            let error = anyhow::Error::from(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"message":"upstream said PERMISSION_DENIED"}"#,
            ));
            let explained = explain_missing_scope(error);
            assert!(!explained.to_string().contains(HINT), "got: {explained}");
        }

        #[test]
        fn an_unrelated_failure_is_left_alone() {
            let error = anyhow::Error::from(ApiError::new(
                StatusCode::BAD_REQUEST,
                r#"{"message":"entry id too long"}"#,
            ));
            let explained = explain_missing_scope(error);
            assert!(!explained.to_string().contains(HINT), "got: {explained}");
        }

        #[test]
        fn the_body_is_found_under_a_context_layer() {
            // `to_string()` renders only the top frame, so a caller that adds
            // context before asking used to lose the hint entirely.
            let error = anyhow::Error::from(ApiError::new(
                StatusCode::FORBIDDEN,
                r#"{"code":"PERMISSION_DENIED"}"#,
            ))
            .context("listing entries");
            let explained = explain_missing_scope(error);
            assert!(explained.to_string().contains(HINT), "got: {explained}");
        }

        #[test]
        fn an_error_with_no_http_status_keeps_the_old_text_only_behaviour() {
            let error = anyhow::anyhow!("the key is missing the universe-datastores scope");
            assert!(explain_missing_scope(error).to_string().contains(HINT));
        }
    }
}
