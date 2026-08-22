//! Reads `rbxplace.toml` and resolves an `--env` to a universe id (plus
//! optionally a place id). Format is shared with `rbx place`, `rbx config`,
//! and every other subcommand in the suite.
//!
//! Example file:
//! ```toml
//! [owner]
//! type = "group"            # or "user"
//! id = 1234567
//!
//! [dev]
//! universe_id = 9876543210
//! # optional per-env override:
//! # owner = { type = "user", id = 42 }
//! [dev.places]
//! main = 123456789012345
//!
//! [prod]
//! universe_id = 9876543211
//! [prod.places]
//! main = 234567890123456
//! lobby = 234567890999999
//! ```
//!
//! Tools that only act at universe level (game passes, badges, products) can
//! ignore `places` entirely via [`resolve_universe_id`]. Tools that need a
//! specific place can call [`resolve`] with an optional place override.
//!
//! The optional top-level `[owner]` block is the single source of truth for
//! "who owns this Roblox project". Other tools (rbx shop, rbx apikey, rbx init)
//! fall back to it when their own owner/creator field is missing. Per-env
//! overrides under `[<env>.owner]` are allowed for the (rare) case where one
//! env lives under a different account than the rest.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};

use crate::owner::Owner;

/// Keys an env table gives meaning to.
///
/// One list for the whole suite, deliberately, rather than deriving it from
/// the structs: three crates deserialize this file into three different
/// shapes, each modelling only the fields it needs, so a per-struct check
/// would call `places` unknown in rbx-config and `owner` unknown in rbx-place.
/// [`unknown_keys`] reads the raw document instead, and checks it against
/// this.
pub const ENV_KEYS: &[&str] = &[
    "universe_id",
    "env",
    "places",
    "owner",
    "confirm",
    "codegen",
];

/// Keys of the reserved top-level `[codegen]` table.
pub const CODEGEN_KEYS: &[&str] = &["output"];

/// Keys of an owner table, top-level or per-env.
pub const OWNER_KEYS: &[&str] = &["type", "id"];

/// Names no env may take.
///
/// `all` is what every command expands to "every env", so a section spelled
/// that way means "the env literally named all" to some commands and "all of
/// them" to others. `owner` and `codegen` are the reserved top-level tables
/// [`PlacesFile`] consumes, so a section under either name is not read as an
/// env at all.
///
/// One list for the whole suite, like [`ENV_KEYS`]: the writers live in
/// rbx-init and rbx-import, and two copies of this rule would let one command
/// write a name another refuses.
pub const RESERVED_ENV_NAMES: &[&str] = &["all", "owner", "codegen"];

/// Whether `name` is one of [`RESERVED_ENV_NAMES`].
pub fn is_reserved_env_name(name: &str) -> bool {
    RESERVED_ENV_NAMES.contains(&name)
}

/// The file's name, and the default value of the global `--places` flag.
pub const PLACES_FILE: &str = "rbxplace.toml";

/// Where `rbxplace.toml` lives for a command that also takes a `--dir`.
///
/// One rule for the whole suite, deliberately: `rbx import` and `rbx check`
/// each grew their own answer, so the same `--dir` moved the implicit lookup
/// for one command and not the other. The rule is that an explicit `--places`
/// wins (a shared env file is often outside the working tree) and otherwise
/// the file sits in `--dir`, alongside everything else the command reads or
/// writes. A `--dir` that names a project directory names the whole project,
/// `rbxplace.toml` included.
///
/// "Explicit" means "not the clap default", which is the only signal available
/// here: `--places rbxplace.toml` typed out by hand is indistinguishable from
/// the default and resolves as one.
pub fn resolve_places_path(places: &Path, dir: &Path) -> PathBuf {
    if places == Path::new(PLACES_FILE) {
        dir.join(PLACES_FILE)
    } else {
        places.to_path_buf()
    }
}

/// A key this build of rbx gives no meaning to.
///
/// Every reader of `rbxplace.toml` swallows unrecognised keys: it has to, or
/// each tool would reject the fields the others own. That is the right parsing
/// rule and the wrong user experience: from the outside, a key that was
/// ignored looks exactly like a key that was honoured. For a tool whose job is
/// guarding generated files against drift, a silently swallowed key that was
/// meant to change what gets generated is the failure mode hardest to notice.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnknownKey {
    /// Table it was found in, spelled as the file spells it: `assets`,
    /// `codegen`, `prod.owner`.
    pub table: String,
    pub key: String,
    /// What that table does accept, for the hint.
    pub known: &'static [&'static str],
}

fn collect_unknown(
    table_name: &str,
    table: &toml::map::Map<String, toml::Value>,
    known: &'static [&'static str],
    out: &mut Vec<UnknownKey>,
) {
    for key in table.keys() {
        if !known.contains(&key.as_str()) {
            out.push(UnknownKey {
                table: table_name.to_string(),
                key: key.clone(),
                known,
            });
        }
    }
}

/// Every key in `content` that no rbx tool reads, sorted.
///
/// Works off the raw document rather than a deserialized struct, so it is the
/// same answer whichever crate is loading. Only the direct children of env and
/// reserved tables are checked: `[<env>.places]` holds arbitrary place names,
/// which are data and not keys.
///
/// A document that does not parse yields nothing: the caller's typed parse
/// reports that failure, with a line number this cannot produce.
pub fn unknown_keys(content: &str) -> Vec<UnknownKey> {
    let Ok(toml::Value::Table(root)) = content.parse::<toml::Value>() else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for (name, value) in &root {
        let Some(table) = value.as_table() else {
            continue;
        };
        match name.as_str() {
            "owner" => collect_unknown(name, table, OWNER_KEYS, &mut found),
            "codegen" => collect_unknown(name, table, CODEGEN_KEYS, &mut found),
            env => {
                collect_unknown(env, table, ENV_KEYS, &mut found);
                // Inline (`owner = { type = "user", id = 42 }`) or a
                // `[<env>.owner]` section, `as_table` covers both.
                if let Some(owner) = table.get("owner").and_then(|v| v.as_table()) {
                    collect_unknown(&format!("{env}.owner"), owner, OWNER_KEYS, &mut found);
                }
            }
        }
    }
    found.sort();
    found
}

/// The warning text for `unknown`, or `None` when there is nothing to say.
///
/// Split out of [`warn_unknown_keys`] so the wording is reachable from a test.
/// The reporting path deduplicates through process-global state, which by
/// construction cannot be asserted on from a parallel test suite: the first
/// test to warn about a path silently mutes every later one. Keeping the
/// formatting pure moves the part worth testing out of that shadow.
pub fn unknown_keys_warning(path: &Path, unknown: &[UnknownKey]) -> Option<String> {
    if unknown.is_empty() {
        return None;
    }

    let mut out = format!(
        "{} {}: {} unrecognised key{}, ignored by rbx {}:\n\n",
        "warning:".yellow().bold(),
        path.display(),
        unknown.len(),
        if unknown.len() == 1 { "" } else { "s" },
        env!("CARGO_PKG_VERSION"),
    );
    for entry in unknown {
        out.push_str(&format!("  [{}] {}\n", entry.table, entry.key.yellow()));
        out.push_str(&format!(
            "    {} {}\n",
            "known keys:".dimmed(),
            entry.known.join(", ")
        ));
    }
    out.push_str(
        "\nAn ignored key changes nothing. Either it is misspelled, or it comes from a\n\
         release newer than this one: check the changelog for the version that\n\
         introduces it before assuming it took effect.",
    );
    Some(out)
}

/// Report ignored keys on stderr, once per path. Never fails the command: a
/// key from a newer release has to stay loadable, or upgrading the file would
/// mean upgrading every machine at the same instant.
///
/// The once-per-path set is process-global, which `api/base.rs` argues against
/// for the API host, and rightly, since a test asserting on the host needs
/// its own. This one stays global because its job *is* to be
/// process-wide: a single command loads `rbxplace.toml` through rbx-core and
/// again through the domain crate's narrower struct, and the user should read
/// the same warning once, not twice. Scoping it to a loader would restore the
/// duplicate it exists to remove. What the global was hiding (the wording)
/// now lives in [`unknown_keys_warning`], where a test can reach it.
pub fn warn_unknown_keys(path: &Path, unknown: &[UnknownKey]) {
    let Some(message) = unknown_keys_warning(path, unknown) else {
        return;
    };

    static WARNED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let already_warned = {
        let mut warned = WARNED
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        !warned.insert(path.to_path_buf())
    };
    if already_warned {
        return;
    }

    eprintln!("{message}");
}

/// Read a `rbxplace.toml` already loaded as text and warn about keys this
/// build does not read. For the loaders in other crates, which parse the file
/// into their own narrower structs.
pub fn warn_unknown_keys_in(path: &Path, content: &str) {
    warn_unknown_keys(path, &unknown_keys(content));
}

/// Say where a colliding name came from, so the fix is obvious without
/// reopening the file: the section's own name, or its `env` override.
fn describe_source(section: &str, resolved: &str) -> &'static str {
    if section == resolved {
        " (section name)"
    } else {
        " (env = \"...\")"
    }
}

/// Reserved `[codegen]` block: where `rbx env gen-module` writes its module.
///
/// The path lives here rather than only in `--out` so the hook, the CI job and
/// the developer cannot disagree about it: a `--check` pointed at a path
/// nobody generates passes green while verifying nothing.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct PlacesCodegen {
    /// Output file path. The format follows the extension: `.lua`, `.luau`,
    /// `.json` or `.ts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<PathBuf>,
}

/// Top-level shape of `rbxplace.toml`. Each top-level table key is an env name.
/// The optional `[owner]` and `[codegen]` tables are consumed as reserved keys
/// (not parsed as envs), so both must be declared before the flattened map.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
pub struct PlacesFile {
    /// Default owner for every env. Per-env `[<env>.owner]` overrides win
    /// when set. See [`PlacesFile::resolve_owner`].
    #[serde(default)]
    pub owner: Option<Owner>,

    /// Codegen output path for `rbx env gen-module`.
    #[serde(default)]
    pub codegen: Option<PlacesCodegen>,

    #[serde(flatten)]
    pub environments: HashMap<String, Environment>,

    /// Keys in the file this build gives no meaning to, collected at load and
    /// already reported on stderr by [`PlacesFile::load`].
    ///
    /// Kept on the struct rather than only printed so `gen-module --check` can
    /// say that an ignored key is a candidate cause of the mismatch: the
    /// generic advice ("regenerate and commit") is wrong in exactly that case.
    #[serde(skip)]
    pub unknown: Vec<UnknownKey>,
}

/// One `[<env>]` table: a named environment pointing at one universe.
///
/// `universe_id` is required; everything else is optional. An unrecognised key
/// is kept rather than rejected, and reported on stderr instead.
//
// The keeping is `_extra` below; the reporting is `unknown_keys`, which reads
// the raw document against `ENV_KEYS`. Kept as a `//` comment rather than a doc
// one: these lines are for someone changing this file, and doc comments here
// are also the hover text in `schemas/rbxplace.schema.json`, where a reader has
// no `_extra` to look at.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
pub struct Environment {
    /// The universe this env targets. Required.
    pub universe_id: u64,

    /// What game code matches on, when it should differ from the section name.
    ///
    /// A rename, not an alias: two envs resolving to the same name is an
    /// error, not two spellings of one env.
    #[serde(default)]
    pub env: Option<String>,

    /// Per-place id map. Tools that operate at universe scope (rbx shop,
    /// rbx meta universe-level fields) leave this empty; tools that need a
    /// place id (rbx place, rbx meta place-level fields) read it.
    #[serde(default)]
    pub places: HashMap<String, u64>,

    /// Per-env owner override. When unset, callers fall back to the top-level
    /// `[owner]` (see [`PlacesFile::resolve_owner`]).
    #[serde(default)]
    pub owner: Option<Owner>,

    /// Whether game code should see this env (default: `true`).
    ///
    /// `false` marks an env that exists for tooling only: a universe you
    /// upload to from CI and never ship. It stays a normal env everywhere
    /// else: `--env` resolves it, `--env all` includes it, `list` and `get`
    /// report it. It is only kept out of the generated modules, so adding one
    /// does not widen the `EnvironmentType` union and force game code to
    /// acknowledge an env it never runs in.
    ///
    /// The trade, and it is real: nothing then maps that universe back to an
    /// env at runtime. Boot the game there and code that resolves its env from
    /// `game.GameId` will not find one. Correct for a universe that only ever
    /// receives uploads, wrong for one that runs gameplay.
    #[serde(default = "default_true")]
    pub codegen: bool,

    /// Prompt before a write to this env: `upload`, `sync`, `rollback`,
    /// `promote`. Defaults to `false`.
    //
    // Typed rather than read stringly out of `_extra`, because `confirm =
    // "true"` (the spelling a YAML habit produces) used to parse happily and
    // then silently disable the prompt on the env most likely to be prod. As a
    // field the wrong type is a load error naming the line, and the schema
    // says `boolean` in the editor before it ever gets that far.
    #[serde(default)]
    pub confirm: bool,

    /// Fields no rbx tool reads, kept so an unknown key does not break
    /// loading. [`unknown_keys`] is what stops them being silent.
    ///
    /// In the JSON Schema this is what keeps `additionalProperties` open on an
    /// env table, which is the whole point: an editor that red-flagged a key
    /// the tool merely warns about would be stricter than the tool, and a
    /// schema stricter than its tool is worse than no schema. `toml::Value`
    /// has no `JsonSchema` impl and would be the wrong shape anyway: the
    /// schema describes the JSON projection of the document.
    #[serde(flatten)]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "HashMap<String, serde_json::Value>")
    )]
    _extra: HashMap<String, toml::Value>,
}

fn default_true() -> bool {
    true
}

impl Environment {
    /// Whether destructive operations on this env prompt first.
    ///
    /// A reader for the [`confirm`](Self::confirm) field, kept because
    /// rbx-env, rbx-meta and rbx-shop call it; they can read the field
    /// directly whenever they are next touched.
    pub fn confirm(&self) -> bool {
        self.confirm
    }
}

impl PlacesFile {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).with_context(|| {
            format!(
                "Failed to read {}. Use --places to point elsewhere, or pass \
                 the universe/place ids directly via per-subcommand flags.",
                path.display()
            )
        })?;
        let mut file: Self = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        file.validate_unique_env_names(path)?;
        file.unknown = unknown_keys(&content);
        warn_unknown_keys(path, &file.unknown);
        Ok(file)
    }

    /// Reject two sections that resolve to the same env name.
    ///
    /// The optional `env` field renames what game code matches on, so the name
    /// a section resolves to is `env` when set and the section name otherwise.
    /// Two sections landing on the same name is not an alias: it is two envs
    /// with one name, and it silently breaks any lookup by name: the generated
    /// module ends up with two entries claiming to be the same env, and a
    /// consumer takes whichever comes first.
    ///
    /// Checked at load so a malformed file fails the same way for every
    /// command, rather than one of them quietly emitting a broken module.
    fn validate_unique_env_names(&self, path: &Path) -> Result<()> {
        // Sorted so the reported pair does not depend on HashMap order.
        let mut sections: Vec<(&str, &str)> = self
            .environments
            .iter()
            .map(|(name, env)| (env.env.as_deref().unwrap_or(name), name.as_str()))
            .collect();
        sections.sort();

        for pair in sections.windows(2) {
            let (resolved, first) = pair[0];
            let (next_resolved, second) = pair[1];
            if resolved != next_resolved {
                continue;
            }
            bail!(
                "{}: two envs resolve to the name '{}'\n  \
                 [{}]{}\n  [{}]{}\n\
                 Env names must be unique: they are what game code matches on.",
                path.display(),
                resolved,
                first,
                describe_source(first, resolved),
                second,
                describe_source(second, resolved),
            );
        }

        Ok(())
    }

    pub fn get(&self, env: &str) -> Result<&Environment> {
        self.environments.get(env).ok_or_else(|| {
            let mut available: Vec<&str> = self.environments.keys().map(|s| s.as_str()).collect();
            available.sort();
            anyhow::anyhow!(
                "Environment '{}' not found in rbxplace.toml.\nAvailable: {}",
                env,
                available.join(", ")
            )
        })
    }

    /// Sorted names of the envs game code should see: everything except
    /// those marked `codegen = false`.
    ///
    /// Deliberately separate from [`env_names`](Self::env_names): `--env all`
    /// still targets a tooling env, because you do want to upload to it. Only
    /// generation filters.
    pub fn codegen_env_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .environments
            .iter()
            .filter(|(_, env)| env.codegen)
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Sorted list of env names. Used by `--env all` expansion.
    pub fn env_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.environments.keys().cloned().collect();
        names.sort();
        names
    }

    /// Resolve the effective owner for `env_name`. Per-env `[<env>.owner]`
    /// wins; otherwise the top-level `[owner]` is returned; otherwise `None`.
    ///
    /// `env_name` is allowed to be missing from the file: callers that hit
    /// this code path before resolving an env (e.g. rbx-shop's standalone
    /// `[experience]` mode) still want the top-level fallback.
    pub fn resolve_owner(&self, env_name: &str) -> Option<&Owner> {
        if let Some(env) = self.environments.get(env_name) {
            if let Some(o) = env.owner.as_ref() {
                return Some(o);
            }
        }
        self.owner.as_ref()
    }
}

/// Resolve `universe_id` for a given env. Use this from tools that don't
/// care about specific places (rbx shop, rbx apikey, parts of rbx meta).
pub fn resolve_universe_id(places_path: &Path, env: &str) -> Result<u64> {
    let places = PlacesFile::load(places_path)?;
    let environment = places.get(env)?;
    Ok(environment.universe_id)
}

/// Resolve `(universe_id, place_id)` for a given env and optional place name.
///
/// `place_override` picks a specific entry from `[<env>.places]`. If omitted,
/// the function picks `main` if it exists, otherwise the only entry, otherwise
/// errors with the list of available place names.
pub fn resolve(places_path: &Path, env: &str, place_override: Option<&str>) -> Result<(u64, u64)> {
    let places = PlacesFile::load(places_path)?;
    let environment = places.get(env)?;

    let place_name = match place_override {
        Some(p) => p,
        None => {
            if environment.places.contains_key("main") {
                "main"
            } else if environment.places.len() == 1 {
                environment
                    .places
                    .keys()
                    .next()
                    .expect("len == 1 just checked, key must exist")
            } else if environment.places.is_empty() {
                bail!(
                    "Environment '{}' has no [<env>.places] entries in rbxplace.toml. \
                     Add one or pass the place id directly via a subcommand flag.",
                    env
                );
            } else {
                let mut names: Vec<&str> = environment.places.keys().map(|s| s.as_str()).collect();
                names.sort();
                bail!(
                    "Environment '{}' has multiple places. Pass --place <name>.\nAvailable: {}",
                    env,
                    names.join(", ")
                );
            }
        }
    };

    let place_id = environment.places.get(place_name).copied().ok_or_else(|| {
        let mut names: Vec<&str> = environment.places.keys().map(|s| s.as_str()).collect();
        names.sort();
        anyhow::anyhow!(
            "Place '{}' not found under [{}.places] in rbxplace.toml.\nAvailable: {}",
            place_name,
            env,
            names.join(", ")
        )
    })?;

    Ok((environment.universe_id, place_id))
}
