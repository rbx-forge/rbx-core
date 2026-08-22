//! Studio's stock templates: the list behind the "New Experience" button.
//!
//! Roblox publishes no template API. What Studio offers is the public games of
//! one account, `998796`, so that account's inventory *is* the list and it is
//! read the same way anybody's games are. Measured 2026-08-20: 40 templates,
//! one page, no credential required.
//!
//! Living in `rbx-core` because two commands need the same number for opposite
//! reasons: `rbx open --new` opens a template, and `rbx init create-universe`
//! asks Roblox to clone one.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::api::{encode_query_value, execute_json, ApiBase};

/// The legacy host that lists a user's games.
pub const GAMES_HOST: &str = "https://games.roblox.com";

/// The account Roblox publishes the Studio templates under.
pub const TEMPLATE_OWNER_USER_ID: u64 = 998796;

/// The stock baseplate, and what Studio's "New Experience" opens by default.
///
/// Measured rather than assumed: clicking that button logs
/// `PlaceManager::createAndShowIDEDoc with task EditPlace` and then
/// `open place (identifier = 95206881)`, so the button is an ordinary
/// `roblox-studio:` open of this place id and nothing more. Studio sets the
/// session's place id to 95206881 to fetch the content and then back to `0`,
/// which is why the result is unbound: the content arrives, the identity does
/// not, and the first save to Roblox has to create the experience.
pub const DEFAULT_TEMPLATE_PLACE_ID: u64 = 95206881;

/// One template, as something openable rather than as Roblox models it.
///
/// Roblox returns universes; what Studio opens is their start place. Carrying
/// the place id only keeps callers from having to remember which of the two
/// numbers in the response is the one they wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StudioTemplate {
    pub place_id: u64,
    pub name: String,
}

#[derive(Debug, Deserialize)]
struct GamesRootPlace {
    #[serde(default)]
    id: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GamesEntry {
    #[serde(default)]
    name: String,
    root_place: Option<GamesRootPlace>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GamesPage {
    data: Vec<GamesEntry>,
    next_page_cursor: Option<String>,
}

/// Every Studio template, baseplate first.
///
/// The baseplate is lifted to the top rather than left where Roblox puts it:
/// the listing comes back newest-first, which buries the one template that is
/// both the default here and the one most people came for.
pub async fn list_templates(
    client: &reqwest::Client,
    games: &ApiBase,
) -> Result<Vec<StudioTemplate>> {
    let mut found: Vec<StudioTemplate> = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let mut url = games.join(&format!(
            "/v2/users/{TEMPLATE_OWNER_USER_ID}/games?accessFilter=Public&limit=50&sortOrder=Asc"
        ));
        if let Some(c) = &cursor {
            // Encoded, not pasted: the cursor is opaque, and a `+` or `&` in it
            // would quietly re-request page one for ever.
            url.push_str("&cursor=");
            url.push_str(&encode_query_value(c));
        }

        let page: GamesPage = execute_json(|| async { Ok(client.get(&url).send().await?) })
            .await
            .context("Failed to list the Studio templates")?;

        for entry in page.data {
            // A template with no start place is one nothing can be opened from,
            // and a nameless row is one nobody can pick out of a menu. Both are
            // dropped rather than shown as an unusable line.
            let Some(place_id) = entry.root_place.map(|p| p.id).filter(|id| *id != 0) else {
                continue;
            };
            let name = entry.name.trim();
            if name.is_empty() {
                continue;
            }
            found.push(StudioTemplate {
                place_id,
                name: name.to_string(),
            });
        }

        match page.next_page_cursor {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => break,
        }
    }

    if let Some(at) = found
        .iter()
        .position(|t| t.place_id == DEFAULT_TEMPLATE_PLACE_ID)
    {
        let baseplate = found.remove(at);
        found.insert(0, baseplate);
    }

    Ok(found)
}
