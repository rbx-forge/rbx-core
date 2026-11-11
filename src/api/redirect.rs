//! Which redirects the shared clients follow, and how a request that carries
//! no credential follows the rest.
//!
//! reqwest's default policy removes `Authorization` and `Cookie` when a
//! redirect changes host, and forwards every other header, `x-api-key`
//! included. An Open Cloud endpoint that redirected to another host would hand
//! it the key. A policy cannot edit the headers of the next hop, only follow,
//! stop or fail, so the shared clients stop at the first hop that leaves the
//! origin: nothing the caller attached reaches another origin, whatever the
//! header is called.
//!
//! A request that has to go further follows its redirects by hand, through
//! [`follow`], with a client that follows none: the credential, if there is
//! one, goes with a hop only while the chain stays on the origin it was given
//! for, and once dropped stays dropped. A download from a CDN is that request,
//! and [`get_without_credentials`] is it with no credential at all.

use anyhow::{bail, Context, Result};
use reqwest::redirect::Policy;
use reqwest::{header, Client, Response, Url};

use super::DEFAULT_REQUEST_TIMEOUT;

/// As many redirects as one request follows: reqwest's own default.
const MAX_REDIRECTS: usize = 10;

/// Follow while the request stays on its origin (scheme, host and port), stop
/// at the first hop that leaves it.
///
/// One move off the origin is followed: the same host going from http to
/// https, both on their default port, which is how a site sends a plain
/// request to its secure address. A hop that leaves https is stopped, even
/// back to the first origin after such an upgrade.
///
/// Stopping hands the 3xx to the caller as the response, rather than failing:
/// the retry layer reports it with its status, and a caller that wants the
/// next hop can read its `Location`.
pub(super) fn same_origin() -> Policy {
    Policy::custom(|attempt| {
        let previous = attempt.previous();
        let next = attempt.url();
        let leaves_origin = previous
            .first()
            .is_none_or(|first| !stays_on_site(first, next))
            || previous.last().is_some_and(|last| downgrades(last, next));
        if leaves_origin {
            attempt.stop()
        } else if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else {
            attempt.follow()
        }
    })
}

/// GET `url` with no credential, following every redirect, to another origin
/// included.
///
/// For a download from the address an API answered with: a CDN, whose URL
/// carries its own signature. The request has nothing on it to leak, so a hop
/// to another host is safe to follow, and a client from
/// [`build_client`](super::build_client) would stop there instead. It is sent
/// with a client of its own, with the timeout of the shared ones, that follows
/// no redirect: this function counts the hops, at most ten.
///
/// The response that ends the chain comes back whatever its status, and
/// checking it is the caller's. A redirect from https to http is refused. No
/// error names the URL, the first or any hop after it, since a signed URL in
/// an error is a working download link in a log: a hop is named by its host.
pub async fn get_without_credentials(url: &str) -> Result<Response> {
    Ok(follow(&no_redirect_client()?, url, None).await?.response)
}

/// The client [`follow`] sends every hop with: it follows no redirect, so
/// each one comes back to the loop that decides where the credential goes.
pub(super) fn no_redirect_client() -> Result<Client> {
    Client::builder()
        .gzip(true)
        .timeout(DEFAULT_REQUEST_TIMEOUT)
        .redirect(Policy::none())
        .build()
        .context("building the HTTP client")
}

/// The response that ended a chain of redirects, and where it came from.
#[derive(Debug)]
pub(super) struct Followed {
    pub(super) response: Response,
    /// Whether the response came from another origin than the first URL's,
    /// after a redirect: its body is then not the endpoint's to show.
    pub(super) answered_elsewhere: bool,
}

/// GET `url`, with `credential` as a header when there is one, following
/// redirects one hop at a time.
///
/// The one hop loop: [`get_without_credentials`] and the asset download both
/// go through it. `client` must follow no redirect itself
/// ([`no_redirect_client`]), or its own hops would carry the credential past
/// this loop and count against nothing.
///
/// The credential goes with a hop only while every hop so far has stayed on
/// the origin it was given for, and once dropped it stays dropped. The same
/// host moved from http to https on default ports counts as that origin, as
/// it does for the shared clients' policy. At most
/// [`MAX_REDIRECTS`] redirects are followed, so at most eleven requests are
/// sent. A redirect from https to http is refused rather than followed, with
/// or without a credential: it would put the rest of the chain, and the URL's
/// signature, on the wire in clear.
pub(super) async fn follow(
    client: &Client,
    url: &str,
    credential: Option<(&str, &str)>,
) -> Result<Followed> {
    // Not `{url}` in any message: a download location is a signed URL.
    let mut url = Url::parse(url).context("the address to download from is not a URL")?;
    let first = url.clone();
    let mut credential = credential;
    let mut redirects = 0;
    loop {
        let mut request = client.get(url.clone());
        if let Some((name, value)) = credential {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(reqwest::Error::without_url)?;
        let answered_elsewhere = !stays_on_site(&first, &url);
        let location = response
            .status()
            .is_redirection()
            .then(|| response.headers().get(header::LOCATION))
            .flatten();
        let Some(location) = location else {
            return Ok(Followed {
                response,
                answered_elsewhere,
            });
        };
        let host = url.host_str().unwrap_or("the server").to_string();
        let next = location
            .to_str()
            .ok()
            .and_then(|location| url.join(location).ok())
            .with_context(|| format!("{host} redirected to an unreadable location"))?;
        if redirects == MAX_REDIRECTS {
            bail!("more than {MAX_REDIRECTS} redirects, the last from {host}");
        }
        if downgrades(&url, &next) {
            bail!(
                "{host} redirected from https to {}, which is not followed",
                next.scheme()
            );
        }
        if !stays_on_site(&first, &next) {
            credential = None;
        }
        redirects += 1;
        url = next;
    }
}

/// What an error keeps of a 3xx that was not followed, in place of its body.
///
/// The body of a redirect often repeats its target, and a target can be a
/// signed URL: a working link in whatever log the error lands in. So it is
/// never read, and the target is named by its host alone. The status stays
/// with the caller, which reports it beside this.
pub(crate) fn unfollowed(response: &Response) -> String {
    let host = response
        .headers()
        .get(header::LOCATION)
        .and_then(|location| location.to_str().ok())
        .and_then(|location| response.url().join(location).ok())
        .and_then(|target| target.host_str().map(str::to_string));
    match host {
        Some(host) => format!("redirected to {host}, which was not followed"),
        None => "redirected, and the redirect was not followed".to_string(),
    }
}

/// Whether `to` is where a credential given for `from` may still go: the same
/// origin, or the same host moved from http to https with both on their
/// default port.
fn stays_on_site(from: &Url, to: &Url) -> bool {
    let upgrade = from.scheme() == "http"
        && to.scheme() == "https"
        && from.host_str().is_some()
        && from.host_str() == to.host_str()
        && from.port().is_none()
        && to.port().is_none();
    from.origin() == to.origin() || upgrade
}

/// Whether a hop from `from` to `to` leaves https.
fn downgrades(from: &Url, to: &Url) -> bool {
    from.scheme() == "https" && to.scheme() != "https"
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::api::{api_status, build_client, build_client_with_user_agent, execute_with_retry};
    use reqwest::StatusCode;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn redirect_to(location: &str) -> ResponseTemplate {
        ResponseTemplate::new(302).insert_header("location", location)
    }

    fn both_clients() -> [Client; 2] {
        [build_client(), build_client_with_user_agent("test-agent")]
    }

    /// Two mock servers listen on two ports, which are two origins.
    async fn redirect_elsewhere() -> (MockServer, MockServer) {
        let api = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/moved"))
            .respond_with(redirect_to(&format!("{}/landing", elsewhere.uri())))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/landing"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&elsewhere)
            .await;
        (api, elsewhere)
    }

    /// The key stays on the origin it was sent to. reqwest's default policy
    /// forwards it, since it drops only `Authorization` and `Cookie`.
    #[tokio::test]
    async fn a_redirect_to_another_origin_does_not_carry_the_key() {
        for client in both_clients() {
            let (api, elsewhere) = redirect_elsewhere().await;

            let response = client
                .get(format!("{}/moved", api.uri()))
                .header("x-api-key", "the-key")
                .send()
                .await
                .unwrap();

            let reached = elsewhere.received_requests().await.unwrap();
            assert!(
                reached.iter().all(|r| r.headers.get("x-api-key").is_none()),
                "the key must not reach another origin"
            );
            assert_eq!(
                response.status(),
                StatusCode::FOUND,
                "the caller sees the redirect it was not taken through"
            );
        }
    }

    /// A stopped redirect whose body repeats its signed target, as S3 and
    /// most servers write one.
    async fn redirect_with_a_signed_body() -> (MockServer, MockServer) {
        let api = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        let target = format!("{}/blob?signature=aaaaBBBBccccDDDD", elsewhere.uri());
        Mock::given(method("GET"))
            .respond_with(
                redirect_to(&target).set_body_string(format!("Found. Redirecting to {target}")),
            )
            .mount(&api)
            .await;
        (api, elsewhere)
    }

    /// Through the retry layer, the stop is an answer with its status, which a
    /// caller can branch on like any refusal. Its body is not kept: the error
    /// names the target's host and nothing else of it.
    #[tokio::test]
    async fn the_retry_layer_reports_a_stopped_redirect_by_its_status_and_host() {
        let (api, _elsewhere) = redirect_with_a_signed_body().await;
        let client = build_client();
        let url = format!("{}/moved", api.uri());

        let error = execute_with_retry(|| async {
            Ok(client.get(&url).header("x-api-key", "k").send().await?)
        })
        .await
        .unwrap_err();

        assert_eq!(api_status(&error), Some(StatusCode::FOUND));
        let shown = format!("{error:#}");
        assert!(!shown.contains("aaaaBBBBccccDDDD"), "got {shown}");
        assert!(!shown.contains("/blob"), "got {shown}");
        assert!(
            shown.contains("redirected to 127.0.0.1, which was not followed"),
            "got {shown}"
        );
    }

    /// The same for a legacy write through the CSRF dance, whose refusal
    /// carries the body to its caller.
    #[tokio::test]
    async fn a_stopped_redirect_through_the_csrf_dance_keeps_no_body() {
        let (api, _elsewhere) = redirect_with_a_signed_body().await;
        let client = build_client();
        let url = format!("{}/moved", api.uri());

        let error = crate::api::send_with_csrf(&crate::api::CsrfToken::new(), || client.get(&url))
            .await
            .unwrap_err();

        let shown = format!("{:#}", anyhow::Error::from(error));
        assert!(shown.contains("302"), "got {shown}");
        assert!(!shown.contains("aaaaBBBBccccDDDD"), "got {shown}");
    }

    /// And for the session check, whose warning quotes what it was answered.
    #[tokio::test]
    async fn a_stopped_redirect_on_the_session_check_keeps_no_body() {
        let (api, _elsewhere) = redirect_with_a_signed_body().await;

        let verdict =
            crate::session::check_with_host(&build_client(), "a-cookie", &api.uri()).await;

        let crate::session::Session::Unknown(why) = verdict else {
            panic!("a redirect is no verdict, got {verdict:?}");
        };
        assert!(why.contains("302"), "got {why}");
        assert!(!why.contains("aaaaBBBBccccDDDD"), "got {why}");
    }

    /// A redirect on the same origin is followed, key and all, so a moved
    /// endpoint keeps working.
    #[tokio::test]
    async fn a_redirect_on_the_same_origin_keeps_the_key() {
        for client in both_clients() {
            let api = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/old"))
                .respond_with(redirect_to("/new"))
                .mount(&api)
                .await;
            Mock::given(method("GET"))
                .and(path("/new"))
                .and(header("x-api-key", "the-key"))
                .respond_with(ResponseTemplate::new(200).set_body_string("here"))
                .mount(&api)
                .await;

            let response = client
                .get(format!("{}/old", api.uri()))
                .header("x-api-key", "the-key")
                .send()
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.text().await.unwrap(), "here");
        }
    }

    #[tokio::test]
    async fn a_redirect_loop_on_the_same_origin_ends_in_an_error() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/loop"))
            .respond_with(redirect_to("/loop"))
            .mount(&api)
            .await;

        let error = build_client()
            .get(format!("{}/loop", api.uri()))
            .send()
            .await
            .unwrap_err();

        assert!(error.is_redirect(), "got {error}");
    }

    /// The CDN hop: no credential on it, so a hop to another origin is
    /// followed, which the shared client alone would stop at.
    #[tokio::test]
    async fn a_request_without_credentials_follows_a_redirect_to_another_origin() {
        let (api, elsewhere) = redirect_elsewhere().await;

        let response = get_without_credentials(&format!("{}/moved", api.uri()))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(elsewhere.received_requests().await.unwrap().len(), 1);
    }

    /// Ten redirects are followed and the eleventh is not, counted one request
    /// at a time: a client that followed redirects itself would send up to ten
    /// for every hop this loop counts.
    #[tokio::test]
    async fn a_loop_ends_after_exactly_ten_redirects() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/a"))
            .respond_with(redirect_to("/a2"))
            .mount(&a)
            .await;
        Mock::given(method("GET"))
            .and(path("/a2"))
            .respond_with(redirect_to(&format!("{}/b", b.uri())))
            .mount(&a)
            .await;
        Mock::given(method("GET"))
            .respond_with(redirect_to(&format!("{}/a", a.uri())))
            .mount(&b)
            .await;

        let error = get_without_credentials(&format!("{}/a", a.uri()))
            .await
            .unwrap_err();

        assert!(
            error.to_string().starts_with("more than 10 redirects"),
            "got {error}"
        );
        let sent =
            a.received_requests().await.unwrap().len() + b.received_requests().await.unwrap().len();
        assert_eq!(
            sent,
            MAX_REDIRECTS + 1,
            "the first request and ten redirects"
        );
    }

    #[tokio::test]
    async fn ten_redirects_reach_the_end_of_the_chain() {
        let server = MockServer::start().await;
        for hop in 0..MAX_REDIRECTS {
            Mock::given(method("GET"))
                .and(path(format!("/{hop}")))
                .respond_with(redirect_to(&format!("/{}", hop + 1)))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path(format!("/{MAX_REDIRECTS}")))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let response = get_without_credentials(&format!("{}/0", server.uri()))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// The credential stays on its origin, and once it has left it does not
    /// come back: a hop back to the first origin from elsewhere was chosen by
    /// the other host.
    #[tokio::test]
    async fn a_followed_credential_stays_on_its_origin_and_does_not_come_back() {
        let api = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(redirect_to("/same"))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/same"))
            .respond_with(redirect_to(&format!("{}/away", elsewhere.uri())))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/away"))
            .respond_with(redirect_to(&format!("{}/back", api.uri())))
            .mount(&elsewhere)
            .await;
        Mock::given(method("GET"))
            .and(path("/back"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&api)
            .await;

        let followed = follow(
            &no_redirect_client().unwrap(),
            &format!("{}/start", api.uri()),
            Some(("x-api-key", "the-key")),
        )
        .await
        .unwrap();

        assert_eq!(followed.response.status(), StatusCode::OK);
        assert!(!followed.answered_elsewhere, "/back is on the first origin");
        let with_key = |requests: Vec<wiremock::Request>| {
            requests
                .into_iter()
                .filter(|r| r.headers.get("x-api-key").is_some())
                .map(|r| r.url.path().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            with_key(api.received_requests().await.unwrap()),
            ["/start", "/same"]
        );
        assert!(with_key(elsewhere.received_requests().await.unwrap()).is_empty());
    }

    #[tokio::test]
    async fn an_answer_from_another_origin_says_so() {
        let (api, _elsewhere) = redirect_elsewhere().await;

        let followed = follow(
            &no_redirect_client().unwrap(),
            &format!("{}/moved", api.uri()),
            None,
        )
        .await
        .unwrap();

        assert!(followed.answered_elsewhere);
    }

    /// The mock servers speak http only, so the upgrade is asserted on the
    /// test the policy and the loop apply to every hop.
    #[test]
    fn the_same_host_moving_to_https_stays_on_site_and_nothing_else_does() {
        let url = |s: &str| Url::parse(s).unwrap();
        let on_site = |from: &str, to: &str| stays_on_site(&url(from), &url(to));
        assert!(on_site("http://api.test/a", "https://api.test/b"));
        assert!(on_site("http://api.test:80/a", "https://api.test:443/b"));
        assert!(on_site("https://api.test/a", "https://api.test/b"));
        assert!(!on_site("https://api.test/a", "http://api.test/a"));
        assert!(!on_site("http://api.test/a", "https://other.test/a"));
        assert!(!on_site("http://api.test:8080/a", "https://api.test/a"));
        assert!(!on_site("http://api.test/a", "https://api.test:8443/a"));
        assert!(!on_site("http://api.test/a", "http://api.test:8080/a"));
    }

    /// The mock servers speak http only, so the downgrade is asserted on the
    /// test the loop applies to every hop.
    #[test]
    fn a_hop_from_https_to_http_is_a_downgrade() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(downgrades(
            &url("https://cdn.test/a"),
            &url("http://cdn.test/a")
        ));
        assert!(downgrades(
            &url("https://cdn.test/a"),
            &url("http://other.test/")
        ));
        assert!(!downgrades(
            &url("https://cdn.test/a"),
            &url("https://other.test/")
        ));
        assert!(!downgrades(
            &url("http://cdn.test/a"),
            &url("https://cdn.test/a")
        ));
        assert!(!downgrades(
            &url("http://cdn.test/a"),
            &url("http://other.test/")
        ));
    }

    /// A signed URL is a working download link, so a failure does not print
    /// it.
    #[tokio::test]
    async fn a_failure_does_not_print_the_url() {
        let error = get_without_credentials("http://127.0.0.1:1/blob?signature=aaaaBBBBccccDDDD")
            .await
            .unwrap_err();

        let shown = format!("{error:#}");
        assert!(!shown.contains("aaaaBBBBccccDDDD"), "got {shown}");
        assert!(!shown.contains("/blob"), "got {shown}");
    }
}
