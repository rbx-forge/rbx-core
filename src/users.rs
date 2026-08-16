//! Turning what a human types into a Roblox user id.
//!
//! Shared rather than owned by one subcommand: anything acting on a player
//! (restrictions, notifications, a datastore entry keyed by user) has the same
//! problem, and none of them should re-solve it.
//!
//! These are the public `users.roblox.com` endpoints, not Open Cloud. Open
//! Cloud can read a user *by id* (`Cloud_GetUser`) but publishes no way to go
//! from a name to an id at all, so there is no version of this that avoids the
//! legacy host. They need no API key, which is why resolution works before any
//! key is configured.

use anyhow::{bail, Result};
use reqwest::Client;
use serde::Deserialize;

use crate::api::execute_json;

/// `pub(crate)` because `session` sends the one authenticated call this host
/// answers, and two literals for one host is how they drift apart.
pub(crate) const USERS_HOST: &str = "https://users.roblox.com";

/// Something a person typed to mean "this player".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserRef {
    Id(u64),
    Name(String),
}

impl UserRef {
    /// Parse one argument.
    ///
    /// Accepted, in the order they are tried:
    ///
    /// ```text
    /// 156                                        an id
    /// builderman                                 a username
    /// name:12345                                 a username, forced
    /// @builderman                                a username, forced
    /// https://www.roblox.com/users/156/profile   a profile link, pasted
    /// ```
    ///
    /// Bare digits are read as an **id**, because that is overwhelmingly what
    /// they are. Roblox does allow an all-digit username, so a forcing prefix
    /// exists to say "no, really, this is a name". Guessing the other way round
    /// would make the common case need punctuation.
    ///
    /// There are two forcing prefixes because `@` is hostile in PowerShell: it
    /// is the splatting operator there, so a bare `@builderman` expands to a
    /// variable that does not exist and the argument silently vanishes before
    /// the program is reached. `"@builderman"` quoted works, and `name:` needs
    /// no quoting in any shell.
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("empty user reference");
        }

        if let Some(id) = parse_profile_url(input) {
            return Ok(Self::Id(id));
        }
        if let Some(name) = input.strip_prefix("name:") {
            if name.is_empty() {
                bail!("`name:` needs a username after it");
            }
            return Ok(Self::Name(name.to_string()));
        }
        if let Some(name) = input.strip_prefix('@') {
            if name.is_empty() {
                bail!("`@` is not a username");
            }
            return Ok(Self::Name(name.to_string()));
        }
        if let Ok(id) = input.parse::<u64>() {
            if id == 0 {
                bail!("0 is not a user id");
            }
            return Ok(Self::Id(id));
        }
        Ok(Self::Name(input.to_string()))
    }
}

/// Pull the id out of a pasted profile link, in any of the forms Roblox serves.
fn parse_profile_url(input: &str) -> Option<u64> {
    let lower = input.to_ascii_lowercase();
    if !lower.contains("roblox.com/users/") {
        return None;
    }
    let after = lower.split("roblox.com/users/").nth(1)?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|id| *id != 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: u64,
    pub name: String,
    pub display_name: String,
    pub has_verified_badge: bool,
}

impl User {
    pub fn profile_url(&self) -> String {
        profile_url(self.id)
    }

    /// A headshot image URL. Public endpoint, no key.
    ///
    /// The image is not rendered anywhere; this is a link to open when you want
    /// to be sure you are looking at the right person before acting on them.
    pub fn thumbnail_url(&self) -> String {
        format!(
            "https://thumbnails.roblox.com/v1/users/avatar-headshot\
             ?userIds={}&size=150x150&format=Png",
            self.id
        )
    }

    /// `builderman (156)`, or `builderman "Builder Man" (156)` when the display
    /// name differs. Impersonation usually shows up as a display name copying
    /// somebody else, so both are printed when they diverge.
    pub fn label(&self) -> String {
        if self.display_name == self.name {
            format!("{} ({})", self.name, self.id)
        } else {
            format!("{} \"{}\" ({})", self.name, self.display_name, self.id)
        }
    }
}

// There is deliberately no thumbnail helper. The obvious endpoint,
// `thumbnails.roblox.com/v1/users/avatar-headshot`, answers with JSON wrapping
// an `imageUrl`, not with an image, so printing it hands someone a link that
// looks like a picture and is not one. Resolving the real url means an extra
// request per user on every write, for something nobody clicks: the profile
// link already answers the only question being asked, which is whether this is
// the right account.
pub fn profile_url(id: u64) -> String {
    format!("https://www.roblox.com/users/{id}/profile")
}

#[derive(Deserialize)]
struct NameLookupResponse {
    #[serde(default)]
    data: Vec<NameLookupEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NameLookupEntry {
    id: u64,
    name: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    has_verified_badge: bool,
    #[serde(default)]
    requested_username: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserByIdResponse {
    id: u64,
    name: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    has_verified_badge: bool,
}

/// Resolve a batch of references to users, in the order given.
///
/// One request for every name and one per id, rather than one per reference.
///
/// A username that does not exist is an **error**, not a silently missing row.
/// The endpoint simply omits unknown names from its response, so asking for
/// three and getting two back is the only signal that one was wrong, and a
/// caller that does not compare would act on the wrong set of people.
pub async fn resolve(client: &Client, refs: &[UserRef]) -> Result<Vec<User>> {
    resolve_with_host(client, refs, USERS_HOST).await
}

/// [`resolve`] against a caller-chosen host, so tests can point it at a mock.
/// Production code should call [`resolve`].
#[doc(hidden)]
pub async fn resolve_with_host(client: &Client, refs: &[UserRef], host: &str) -> Result<Vec<User>> {
    let names: Vec<&str> = refs
        .iter()
        .filter_map(|reference| match reference {
            UserRef::Name(name) => Some(name.as_str()),
            UserRef::Id(_) => None,
        })
        .collect();

    let mut by_name: Vec<User> = Vec::new();
    if !names.is_empty() {
        let url = format!("{host}/v1/usernames/users");
        let body = serde_json::json!({ "usernames": names, "excludeBannedUsers": false });
        let response: NameLookupResponse = execute_json(|| {
            let request = client.post(&url).json(&body);
            async move { request.send().await.map_err(Into::into) }
        })
        .await?;

        for entry in response.data {
            by_name.push(User {
                id: entry.id,
                display_name: if entry.display_name.is_empty() {
                    entry.name.clone()
                } else {
                    entry.display_name.clone()
                },
                name: entry
                    .requested_username
                    .filter(|requested| requested.eq_ignore_ascii_case(&entry.name))
                    .map(|_| entry.name.clone())
                    .unwrap_or(entry.name),
                has_verified_badge: entry.has_verified_badge,
            });
        }

        let missing: Vec<&str> = names
            .iter()
            .filter(|wanted| {
                !by_name
                    .iter()
                    .any(|user| user.name.eq_ignore_ascii_case(wanted))
            })
            .copied()
            .collect();
        if !missing.is_empty() {
            bail!(
                "no Roblox user is named: {}. Usernames are not display names; \
                 pass a user id if you are unsure.",
                missing.join(", ")
            );
        }
    }

    let mut out = Vec::with_capacity(refs.len());
    for reference in refs {
        match reference {
            UserRef::Name(name) => {
                let found = by_name
                    .iter()
                    .find(|user| user.name.eq_ignore_ascii_case(name))
                    .expect("every name was checked above");
                out.push(found.clone());
            }
            UserRef::Id(id) => out.push(fetch_by_id(client, host, *id).await?),
        }
    }
    Ok(out)
}

async fn fetch_by_id(client: &Client, host: &str, id: u64) -> Result<User> {
    let url = format!("{host}/v1/users/{id}");
    let response: UserByIdResponse = execute_json(|| {
        let request = client.get(&url);
        async move { request.send().await.map_err(Into::into) }
    })
    .await?;
    Ok(User {
        id: response.id,
        display_name: if response.display_name.is_empty() {
            response.name.clone()
        } else {
            response.display_name
        },
        name: response.name,
        has_verified_badge: response.has_verified_badge,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_digits_are_an_id() {
        assert_eq!(UserRef::parse("156").unwrap(), UserRef::Id(156));
    }

    #[test]
    fn an_at_prefix_forces_a_username_even_when_it_is_all_digits() {
        // Roblox permits an all-digit username, so there has to be a way to say
        // "this is a name" without ambiguity.
        assert_eq!(
            UserRef::parse("@12345").unwrap(),
            UserRef::Name("12345".into())
        );
    }

    #[test]
    fn a_name_prefix_forces_a_username_without_needing_a_shell_quote() {
        // `@` is the splatting operator in PowerShell, where a bare
        // `@builderman` expands to nothing and the argument disappears before
        // the program sees it. This prefix is safe in every shell.
        assert_eq!(
            UserRef::parse("name:12345").unwrap(),
            UserRef::Name("12345".into())
        );
        assert_eq!(
            UserRef::parse("name:builderman").unwrap(),
            UserRef::Name("builderman".into())
        );
    }

    #[test]
    fn a_forcing_prefix_with_nothing_after_it_is_rejected() {
        assert!(UserRef::parse("name:").is_err());
        assert!(UserRef::parse("@").is_err());
    }

    #[test]
    fn a_bare_word_is_a_username() {
        assert_eq!(
            UserRef::parse("builderman").unwrap(),
            UserRef::Name("builderman".into())
        );
    }

    #[test]
    fn a_pasted_profile_link_yields_the_id() {
        for link in [
            "https://www.roblox.com/users/156/profile",
            "http://roblox.com/users/156",
            "www.roblox.com/users/156/profile#!/about",
            "https://WWW.ROBLOX.COM/users/156/profile",
        ] {
            assert_eq!(UserRef::parse(link).unwrap(), UserRef::Id(156), "{link}");
        }
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(UserRef::parse("  156  ").unwrap(), UserRef::Id(156));
    }

    #[test]
    fn nonsense_references_are_rejected_rather_than_guessed_at() {
        assert!(UserRef::parse("").is_err());
        assert!(UserRef::parse("   ").is_err());
        assert!(UserRef::parse("@").is_err());
        assert!(UserRef::parse("0").is_err());
    }

    #[test]
    fn a_label_shows_the_display_name_only_when_it_differs() {
        let same = User {
            id: 156,
            name: "builderman".into(),
            display_name: "builderman".into(),
            has_verified_badge: true,
        };
        assert_eq!(same.label(), "builderman (156)");

        let different = User {
            display_name: "Builder Man".into(),
            ..same
        };
        assert_eq!(different.label(), "builderman \"Builder Man\" (156)");
    }

    #[test]
    fn a_profile_url_points_at_the_public_profile() {
        assert_eq!(profile_url(156), "https://www.roblox.com/users/156/profile");
    }
}
