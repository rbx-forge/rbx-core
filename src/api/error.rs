//! The typed form of "Roblox answered, and the answer was not a success".
//!
//! Every non-success response used to be flattened into an `anyhow!` string
//! (`API error 404: {body}`) at the moment it was detected, and callers that
//! needed to branch on the status recovered it with `error.to_string()
//! .contains("404")`. That works until a body contains the digits you are
//! looking for. `rbx-ops data get` on an entry whose stored JSON mentions 404
//! reported the entry as missing, because the substring search cannot tell the
//! status line from the payload that follows it; the reverse also happened,
//! where a 500 carrying the text of an upstream 404 was swallowed as "no such
//! key". The status is structured information and destroying it at the
//! boundary is what made those bugs possible.
//!
//! [`ApiError`] keeps the status as a [`StatusCode`]. The [`fmt::Display`]
//! output is byte-identical to the old string, so nothing user-facing moves
//! and no existing message-matching test changes meaning.
//!
//! Domain crates keep their `anyhow::Result` signatures. An `ApiError`
//! converts into `anyhow::Error` on the `?`, and [`api_status`] walks back
//! down the chain to find it, so a caller can branch on the status without a
//! single function signature changing.

use std::fmt;

use reqwest::StatusCode;

/// A non-success HTTP response from a Roblox API.
///
/// Constructed by the retry layer when the response is not retryable, or when
/// the retries are spent — so an `ApiError` carrying 429 means the rate limit
/// survived the whole backoff schedule, not that one request was throttled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    status: StatusCode,
    body: String,
}

impl ApiError {
    /// `body` is the raw response text. It is kept verbatim rather than parsed
    /// because Open Cloud is inconsistent about whether a failure is JSON, and
    /// the raw text is what makes an unrecognised shape diagnosable.
    pub fn new(status: StatusCode, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    /// The HTTP status Roblox returned.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The raw response body.
    pub fn body(&self) -> &str {
        &self.body
    }
}

impl fmt::Display for ApiError {
    /// Deliberately identical to the `bail!` this type replaced. The status
    /// and body are both in the message because most callers never branch on
    /// the status and only ever print it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "API error {}: {}", self.status, self.body)
    }
}

impl std::error::Error for ApiError {}

/// The status Roblox returned, if this error came from an HTTP response.
///
/// Walks the whole `anyhow` source chain rather than downcasting the outermost
/// error, because by the time a caller inspects one it has usually collected a
/// `.context()` or two on the way up — `rbx-ops ban` adds one before checking
/// for 429. A direct downcast would see the context string and miss the
/// `ApiError` underneath it.
///
/// Returns `None` for anything that never was an HTTP response: a config
/// error, a JSON parse failure, an argument the user got wrong.
///
/// ```
/// # use rbx_core::api::{api_status, ApiError};
/// # use reqwest::StatusCode;
/// let error = anyhow::Error::from(ApiError::new(StatusCode::NOT_FOUND, "{}"))
///     .context("reading the entry");
/// assert_eq!(api_status(&error), Some(StatusCode::NOT_FOUND));
///
/// assert_eq!(api_status(&anyhow::anyhow!("no api key")), None);
/// ```
pub fn api_status(error: &anyhow::Error) -> Option<StatusCode> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ApiError>())
        .map(ApiError::status)
}

/// Whether this error is an HTTP failure with exactly `status`.
///
/// The common case of [`api_status`], spelled so the call site reads as the
/// question it is asking.
///
/// ```
/// # use rbx_core::api::{is_api_status, ApiError};
/// # use reqwest::StatusCode;
/// let error = anyhow::Error::from(ApiError::new(StatusCode::NOT_FOUND, "{}"));
/// assert!(is_api_status(&error, StatusCode::NOT_FOUND));
/// assert!(!is_api_status(&error, StatusCode::FORBIDDEN));
/// ```
pub fn is_api_status(error: &anyhow::Error, status: StatusCode) -> bool {
    api_status(error) == Some(status)
}

/// The sentence worth showing out of a Roblox legacy error body.
///
/// The `develop`/`groups`/`universes` families answer failures with an
/// envelope rather than a flat message:
///
/// ```json
/// {"errors":[{"code":5,"message":"...","userFacingMessage":"..."}]}
/// ```
///
/// Printing that raw puts JSON punctuation in front of the one clause the
/// user needs. `userFacingMessage` is preferred because Roblox writes it for
/// exactly this purpose; `message` is the fallback, and some endpoints put it
/// at the top level instead of inside `errors`.
///
/// Returns `None` when the body is not that shape — an HTML error page, an
/// empty body, a flat string — so the caller can fall back to showing it
/// whole rather than swallowing it.
///
/// ```
/// # use rbx_core::api::roblox_message;
/// let body = r#"{"errors":[{"userFacingMessage":"You do not have permission."}]}"#;
/// assert_eq!(roblox_message(body).as_deref(), Some("You do not have permission."));
/// assert_eq!(roblox_message("<html>502</html>"), None);
/// ```
pub fn roblox_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let first_error = value
        .get("errors")
        .and_then(|errors| errors.as_array())
        .and_then(|errors| errors.first());

    // `userFacingMessage` first wherever it appears, then `message`. Checked
    // in that order across both shapes rather than per-shape, so an envelope
    // carrying only `message` does not shadow a top-level `userFacingMessage`.
    for key in ["userFacingMessage", "message"] {
        if let Some(text) = first_error
            .and_then(|e| e.get(key))
            .and_then(|m| m.as_str())
        {
            return Some(text.to_string());
        }
        if let Some(text) = value.get(key).and_then(|m| m.as_str()) {
            return Some(text.to_string());
        }
    }
    None
}

/// An [`ApiError`] carrying Roblox's own sentence instead of its JSON.
///
/// For the legacy hosts, whose failures arrive in the envelope
/// [`roblox_message`] understands. Open Cloud proper returns a flatter shape
/// and is constructed with [`ApiError::new`] directly.
///
/// The status is kept either way — that is the whole point of the type — so a
/// caller can still branch on 403 even though what it prints is prose.
pub fn roblox_error(status: StatusCode, body: &str) -> anyhow::Error {
    let message = roblox_message(body).unwrap_or_else(|| body.to_string());
    ApiError::new(status, message).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_matches_the_string_this_type_replaced() {
        // Asserted rather than assumed: several crates' tests match on this
        // exact rendering, and users read it in the terminal.
        let error = ApiError::new(StatusCode::NOT_FOUND, "{\"message\":\"gone\"}");
        assert_eq!(
            error.to_string(),
            "API error 404 Not Found: {\"message\":\"gone\"}"
        );
    }

    #[test]
    fn status_survives_conversion_into_anyhow() {
        let error = anyhow::Error::from(ApiError::new(StatusCode::FORBIDDEN, "denied"));
        assert_eq!(api_status(&error), Some(StatusCode::FORBIDDEN));
    }

    #[test]
    fn status_survives_layers_of_context() {
        // rbx-ops ban wraps twice before it asks about 429. If this breaks,
        // that command silently loses its rate-limit advice.
        let error = anyhow::Error::from(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "slow down"))
            .context("banning user 1")
            .context("applying restrictions");
        assert_eq!(api_status(&error), Some(StatusCode::TOO_MANY_REQUESTS));
    }

    #[test]
    fn a_body_mentioning_a_status_does_not_become_that_status() {
        // The bug this module exists to kill. Before the typed status, this
        // 500 read as a 404 to anyone doing `.to_string().contains("404")`,
        // and `rbx-ops data get` reported a live entry as missing.
        let error = anyhow::Error::from(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upstream returned 404 while resolving the entry",
        ));
        assert_eq!(api_status(&error), Some(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_api_status(&error, StatusCode::NOT_FOUND));
        // ...even though the rendered message still contains the digits.
        assert!(error.to_string().contains("404"));
    }

    #[test]
    fn a_non_http_error_has_no_status() {
        let error = anyhow::anyhow!("Roblox Open Cloud API key required.");
        assert_eq!(api_status(&error), None);
        assert!(!is_api_status(&error, StatusCode::NOT_FOUND));
    }

    #[test]
    fn the_body_is_kept_verbatim() {
        let error = ApiError::new(StatusCode::BAD_REQUEST, "  not json at all  ");
        assert_eq!(error.body(), "  not json at all  ");
    }

    mod roblox_envelope {
        use super::*;

        #[test]
        fn the_user_facing_message_wins_over_the_internal_one() {
            let body = r#"{"errors":[{"code":5,"message":"InsufficientPermissions",
                           "userFacingMessage":"You do not have permission."}]}"#;
            assert_eq!(
                roblox_message(body).as_deref(),
                Some("You do not have permission.")
            );
        }

        #[test]
        fn message_is_used_when_there_is_no_user_facing_one() {
            let body = r#"{"errors":[{"code":5,"message":"InsufficientPermissions"}]}"#;
            assert_eq!(
                roblox_message(body).as_deref(),
                Some("InsufficientPermissions")
            );
        }

        #[test]
        fn a_top_level_message_is_found_too() {
            // Some endpoints skip the envelope. rbx-apikey handled this and
            // rbx-init did not; merging the two implementations must not drop
            // the case.
            let body = r#"{"message":"The API key is invalid."}"#;
            assert_eq!(
                roblox_message(body).as_deref(),
                Some("The API key is invalid.")
            );
        }

        #[test]
        fn an_empty_errors_array_is_not_a_message() {
            assert_eq!(roblox_message(r#"{"errors":[]}"#), None);
        }

        #[test]
        fn a_body_that_is_not_json_yields_nothing() {
            assert_eq!(roblox_message("<html><body>502</body></html>"), None);
            assert_eq!(roblox_message(""), None);
        }

        #[test]
        fn the_raw_body_survives_when_there_is_no_message_to_extract() {
            // Falling back to the whole body matters more than looking tidy:
            // an unparsed body is the only clue left when Roblox answers with
            // something nobody anticipated.
            let error = roblox_error(StatusCode::BAD_GATEWAY, "<html>502</html>");
            assert!(error.to_string().contains("<html>502</html>"));
            assert_eq!(api_status(&error), Some(StatusCode::BAD_GATEWAY));
        }

        #[test]
        fn the_status_is_still_recoverable_from_a_prettified_error() {
            let error = roblox_error(
                StatusCode::FORBIDDEN,
                r#"{"errors":[{"userFacingMessage":"Nope."}]}"#,
            );
            assert!(is_api_status(&error, StatusCode::FORBIDDEN));
            assert!(error.to_string().contains("Nope."));
            // The JSON punctuation is what we were trying to get rid of.
            assert!(!error.to_string().contains("userFacingMessage"));
        }
    }
}
