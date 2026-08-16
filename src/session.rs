//! Is the cookie still a session? Asked once, before anything is written.
//!
//! Resolving a cookie answers "is there one" and nothing else: there is no
//! shape check on the way in and no expiry to read off the value, because a
//! `.ROBLOSECURITY` string carries neither. So until this module existed, the
//! first thing that noticed an expired session was Roblox, in the middle of an
//! operation. `rbx meta sync` sends its Open Cloud patches before its
//! cookie-only ones, which made a stale cookie leave the Open Cloud half
//! applied and the legacy half not — an intermediate state that is real, and
//! on a live universe visible to players until somebody re-runs (#63).
//!
//! One `users/authenticated` call before the first cookie-bearing write turns
//! that into a refusal that changes nothing. Three rules shape what is here:
//!
//! - **Only where a write is actually coming.** A command that attaches a
//!   cookie to a read, or attaches one only if it happens to have it, does not
//!   pay a round trip: it has nothing to leave half-done. `docs/cookie.md`
//!   lists which commands are which.
//! - **Once per execution.** A run that consumes the cookie in three steps
//!   asks once; every later ask reads the verdict off the cache below.
//! - **No answer is not a "no".** An unreachable host, a 5xx or a rate limit
//!   mean this check did not run. Turning a flat network into "your session
//!   expired, sign in again" sends somebody to re-authenticate a session that
//!   was fine, which is a worse error than the one being fixed.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use reqwest::{header, Client, StatusCode};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::api::roblox_message;
use crate::users::USERS_HOST;

/// What `rbx` says when Roblox refuses the cookie.
///
/// It names the state ("expired") and the two ways out, because "401" is not
/// something a user can act on. Renewing is deliberately spelled in both
/// directions: signing in to Studio again is what fixes an auto-detected
/// cookie, and a fresh `--cookie` / `RBX_COOKIE` is what fixes CI, where there
/// is no Studio to sign in to.
pub const EXPIRED_SESSION: &str =
    "the Roblox session has expired: Roblox refused the .ROBLOSECURITY cookie, so nothing was \
     applied. Renew it — sign in to Roblox Studio again, or pass a fresh --cookie / set \
     RBX_COOKIE — then re-run.";

/// What `rbx` says when the cookie is present but empty.
///
/// `RBX_COOKIE=` is an explicit "no cookie" that resolution deliberately keeps
/// as an answer rather than an absence, so it arrives here as a `Some("")`.
/// Sending it and reading back a refusal would report an expired session,
/// which is the one thing it is not.
pub const EMPTY_COOKIE: &str =
    "the Roblox session cookie is empty. RBX_COOKIE= (or --cookie \"\") means \"no cookie\", and \
     this command writes fields that need one. Set RBX_COOKIE to a real value, pass --cookie, or \
     sign in to Roblox Studio and drop RBX_COOKIE so it can be detected.";

/// The account a cookie signs in as.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAccount {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub display_name: String,
}

impl SessionAccount {
    /// `builderman (156)`, or just the id when the endpoint sent no name.
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            self.id.to_string()
        } else {
            format!("{} ({})", self.name, self.id)
        }
    }
}

/// What one `users/authenticated` call concluded.
///
/// Four outcomes rather than a `bool`, because the three that are not "valid"
/// call for three different things: a refusal is the user's to fix, an empty
/// cookie is a configuration mistake with a different remedy, and an
/// unanswered check is nobody's fault and must not be reported as either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    /// Roblox answered and named the account.
    Valid(SessionAccount),
    /// Roblox refused the cookie. The session is over.
    Refused,
    /// There is a cookie value and it is empty, so no call was made.
    Empty,
    /// Nothing could be concluded, with the reason phrased as what happened.
    /// Not a verdict on the session.
    Unknown(String),
}

impl Session {
    /// Whether this outcome should stop a write.
    ///
    /// Only a refusal and an empty value do. `Unknown` deliberately does not:
    /// see the module docs.
    pub fn blocks_a_write(&self) -> bool {
        matches!(self, Session::Refused | Session::Empty)
    }
}

/// Build a `Cookie:` header value out of a raw `.ROBLOSECURITY` string.
///
/// The one normalisation this toolkit applies to a cookie: Roblox wants the
/// `.ROBLOSECURITY=` prefix, users paste the value with or without it. Shared
/// so the check and the calls it vouches for send byte-identical headers —
/// validating one form and then sending another would vouch for nothing.
pub fn cookie_header(raw: &str) -> String {
    if raw.starts_with(".ROBLOSECURITY=") {
        raw.to_string()
    } else {
        format!(".ROBLOSECURITY={raw}")
    }
}

/// Verdicts already reached in this process, keyed by cookie and host.
///
/// Keyed by a hash rather than the value: this map outlives every scope the
/// cookie passes through, and a credential kept alive in a `static` for the
/// life of the process is exactly what `docs/cookie.md` promises does not
/// happen. The host is part of the key so a test pointing at a mock cannot be
/// answered by a verdict another test reached elsewhere.
///
/// A `tokio` mutex rather than a `std` one because it is held across the
/// request: two concurrent asks for the same cookie must produce one call, not
/// two that race to fill the same slot.
static VERDICTS: Mutex<BTreeMap<[u8; 32], Session>> = Mutex::const_new(BTreeMap::new());

fn key(host: &str, cookie: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(host.as_bytes());
    hasher.update(&[0]);
    hasher.update(cookie.as_bytes());
    *hasher.finalize().as_bytes()
}

/// Ask Roblox who this cookie signs in as, at most once per process.
///
/// `client` is the caller's own client rather than one built here, so the
/// check travels with the same user agent and timeout as the calls it is
/// vouching for.
pub async fn check(client: &Client, cookie: &str) -> Session {
    check_with_host(client, cookie, USERS_HOST).await
}

/// `check` against a caller-chosen host, so tests can point it at a mock.
/// Production code calls `check`.
#[doc(hidden)]
pub async fn check_with_host(client: &Client, cookie: &str, host: &str) -> Session {
    if cookie.trim().is_empty() {
        return Session::Empty;
    }

    let mut verdicts = VERDICTS.lock().await;
    let key = key(host, cookie);
    if let Some(known) = verdicts.get(&key) {
        return known.clone();
    }

    let verdict = ask(client, cookie, host).await;
    // Announced here rather than at the call sites, so a run whose check could
    // not answer says so once however many steps consume the cookie.
    if let Session::Unknown(why) = &verdict {
        eprintln!("warning: could not check the Roblox session ({why}); continuing anyway");
    }
    verdicts.insert(key, verdict.clone());
    verdict
}

async fn ask(client: &Client, cookie: &str, host: &str) -> Session {
    let url = format!("{host}/v1/users/authenticated");
    let response = match client
        .get(&url)
        .header(header::COOKIE, cookie_header(cookie))
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => return Session::Unknown(format!("{host} was not reached: {e}")),
    };

    let status = response.status();
    // 401 is the only status that means "this session is over". Everything
    // else non-success is Roblox declining to answer the question for reasons
    // of its own — a challenge, a rate limit, an outage — and none of those is
    // a reason to tell somebody to re-authenticate.
    if status == StatusCode::UNAUTHORIZED {
        return Session::Refused;
    }
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = roblox_message(&body).unwrap_or_else(|| body.trim().to_string());
        return Session::Unknown(if detail.is_empty() {
            format!("{host} answered {status}")
        } else {
            format!("{host} answered {status}: {detail}")
        });
    }

    match serde_json::from_str::<SessionAccount>(&body) {
        Ok(account) => Session::Valid(account),
        Err(e) => Session::Unknown(format!(
            "{host} answered 200 with something unreadable: {e}"
        )),
    }
}

/// The account this cookie signs in as, if the check already knows.
///
/// Reads the cached verdict and **never issues a request**: a command that has
/// called `require_valid` has already paid for the answer, and one that has not
/// gets `None` rather than a surprise round trip in front of a prompt.
///
/// This exists so a confirmation can name the *account* rather than only the
/// resource. Auto-detection silently follows whichever account Studio is signed
/// into, so "wrong account" is the realistic mistake here, and it is the one a
/// prompt about a group id cannot catch. `docs/cookie.md` files it under what
/// is not checked; naming it is the cheap half of checking it.
pub async fn known_account(cookie: &str) -> Option<SessionAccount> {
    known_account_with_host(cookie, USERS_HOST).await
}

/// `known_account` against a caller-chosen host, so tests can point it at a
/// mock. Production code calls `known_account`.
#[doc(hidden)]
pub async fn known_account_with_host(cookie: &str, host: &str) -> Option<SessionAccount> {
    if cookie.trim().is_empty() {
        return None;
    }
    let verdicts = VERDICTS.lock().await;
    match verdicts.get(&key(host, cookie)) {
        Some(Session::Valid(account)) => Some(account.clone()),
        _ => None,
    }
}

/// Prefix a confirmation with the account it will act as.
///
/// `None` leaves the question exactly as it was, so a command with no cookie,
/// or one whose check could not answer, prompts the way it always did rather
/// than claiming an identity nothing established.
pub fn as_account(account: Option<&SessionAccount>, question: &str) -> String {
    match account {
        Some(a) => format!("As {} — {}", a.label(), lower_first(question)),
        None => question.to_string(),
    }
}

/// `Create universe ...` -> `create universe ...`, so the prefix reads as one
/// sentence.
///
/// Lowered only when the second character is itself lowercase, which is what
/// an ordinary capitalised sentence looks like. `VIPPass will be created?`
/// keeps its capital, because a question opening on a resource name or an
/// acronym must survive untouched — misnaming the thing being confirmed is a
/// worse outcome than a capital letter mid-sentence, and this text is read by
/// somebody deciding whether to approve it.
fn lower_first(s: &str) -> String {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let rest = chars.as_str();
    let ordinary_sentence =
        first.is_ascii_uppercase() && rest.chars().next().is_some_and(|c| c.is_ascii_lowercase());
    if ordinary_sentence {
        first.to_ascii_lowercase().to_string() + rest
    } else {
        s.to_string()
    }
}

/// Refuse to go on when the session is not usable.
///
/// The gate the cookie-bearing writes call before their first write. `Ok` for
/// a valid session and for a check that could not run; an error, naming the
/// state and the way out, for a refusal or an empty value.
pub async fn require_valid(client: &Client, cookie: &str) -> Result<()> {
    require_valid_with_host(client, cookie, USERS_HOST).await
}

/// `require_valid` against a caller-chosen host, so tests can point it at a
/// mock. Production code calls `require_valid`.
#[doc(hidden)]
pub async fn require_valid_with_host(client: &Client, cookie: &str, host: &str) -> Result<()> {
    match check_with_host(client, cookie, host).await {
        Session::Valid(_) | Session::Unknown(_) => Ok(()),
        Session::Refused => bail!(EXPIRED_SESSION),
        Session::Empty => bail!(EMPTY_COOKIE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_raw_value_gains_the_prefix_and_a_prefixed_one_does_not_gain_a_second() {
        assert_eq!(cookie_header("ABC"), ".ROBLOSECURITY=ABC");
        assert_eq!(
            cookie_header(".ROBLOSECURITY=ABC"),
            ".ROBLOSECURITY=ABC",
            "prefixing twice sends a cookie Roblox cannot read"
        );
    }

    #[test]
    fn only_a_refusal_and_an_empty_value_stop_a_write() {
        assert!(Session::Refused.blocks_a_write());
        assert!(Session::Empty.blocks_a_write());
        assert!(!Session::Unknown("offline".into()).blocks_a_write());
        assert!(!Session::Valid(SessionAccount {
            id: 1,
            name: "a".into(),
            display_name: "a".into(),
        })
        .blocks_a_write());
    }

    #[test]
    fn an_account_with_no_name_still_labels_as_something() {
        let account = SessionAccount {
            id: 156,
            name: String::new(),
            display_name: String::new(),
        };
        assert_eq!(account.label(), "156");
    }

    /// Two cookies must not share a verdict, or a switching the signed-in Studio account between runs of
    /// the same process would be answered with the previous account's.
    #[test]
    fn the_cache_key_separates_cookies_and_hosts() {
        assert_ne!(key("https://a.test", "one"), key("https://a.test", "two"));
        assert_ne!(key("https://a.test", "one"), key("https://b.test", "one"));
        assert_eq!(key("https://a.test", "one"), key("https://a.test", "one"));
    }

    /// An empty cookie is answered without asking anybody: there is nothing to
    /// ask about, and the remedy is different from an expired session's.
    #[tokio::test]
    async fn an_empty_cookie_needs_no_round_trip() {
        let client = crate::api::build_client();
        assert_eq!(
            check_with_host(&client, "   ", "http://127.0.0.1:1").await,
            Session::Empty
        );
        let error = require_valid_with_host(&client, "", "http://127.0.0.1:1")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("empty"), "got {error}");
        assert!(!error.contains("expired"), "got {error}");
    }

    fn account(name: &str, id: u64) -> SessionAccount {
        SessionAccount {
            id,
            name: name.to_string(),
            display_name: name.to_string(),
        }
    }

    #[test]
    fn a_named_account_is_prefixed_and_the_question_reads_as_one_sentence() {
        let q = as_account(
            Some(&account("builderman", 156)),
            "Create universe 'My Game' under group 42?",
        );
        assert_eq!(
            q,
            "As builderman (156) — create universe 'My Game' under group 42?"
        );
    }

    #[test]
    fn no_account_leaves_the_question_byte_for_byte() {
        // A command with no cookie, or one whose check could not answer, must
        // prompt exactly as it always did rather than claim an identity
        // nothing established.
        let original = "Create universe 'My Game' under group 42?";
        assert_eq!(as_account(None, original), original);
    }

    #[test]
    fn only_a_leading_ascii_capital_is_lowered() {
        // The prefix reads as one sentence, so `Create` becomes `create`. A
        // question opening on a name or an id must survive untouched, because
        // lowering those would misname the thing being confirmed.
        let a = account("builderman", 156);
        assert!(as_account(Some(&a), "VIPPass will be created?").contains("— VIPPass"));
        assert!(as_account(Some(&a), "1234 will be renamed?").contains("— 1234"));
    }

    #[tokio::test]
    async fn an_unchecked_cookie_has_no_known_account_and_makes_no_request() {
        // The point of reading the cache rather than asking: a caller that has
        // not run the check gets None instead of a surprise round trip in
        // front of a prompt. The host here has no server behind it, so a
        // request would hang or fail rather than return None.
        assert!(
            known_account_with_host("never-checked", "http://127.0.0.1:1")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_empty_cookie_has_no_known_account() {
        assert!(known_account_with_host("", "http://127.0.0.1:1")
            .await
            .is_none());
    }
}
