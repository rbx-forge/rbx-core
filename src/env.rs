//! Global CLI flags and env target resolution shared across every subcommand
//! of the `rbx` binary.
//!
//! [`GlobalFlags`] is embedded into the top-level binary's parser via
//! `#[command(flatten)]`. Subcommands receive a `&GlobalFlags` reference and
//! use it to resolve `--env <name>` (and `--env all`) into a list of
//! [`EnvTarget`]s, then call the relevant Open Cloud APIs against each target.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::places::{self, EnvSelector, PlacesFile, ALL_ENVS};

/// Env name domain crates should use when no `--env` is passed and they
/// fall back to a standalone config block (e.g. `[experience]` in
/// `rbxshop.toml`). Centralised here so every domain's lockfile sectioning
/// stays in sync (`[envs.default]` everywhere).
pub const DEFAULT_ENV: &str = "default";

/// What the tool says the first time it uses a cookie nobody asked it to use.
///
/// A `.ROBLOSECURITY` cookie is a full account credential: strictly more
/// powerful than any scoped API key, and not revocable per-tool. Reading one
/// out of a local Studio install is the right convenience for personal use and
/// the wrong thing to do *silently* in a binary other people install: it is
/// the first behaviour a security-minded evaluator looks for, and doing it
/// quietly is inconsistent with the least-privilege posture this project
/// argues for in `docs/ops.md`.
///
/// Announcing costs one line on stderr and turns "it read my session cookie"
/// into "it told me it read my session cookie, and how to stop it".
///
/// The username is deliberately absent, despite being the obvious thing to
/// add: `resolve_cookie` has no access to it, and fetching it would mean a
/// network round-trip (`authenticated_user`) on every command that touches a
/// cookie. Not worth the price of a nicer sentence.
///
/// rbx-apikey prints this same string at its own fallback site. The wording is
/// shared so a user who meets it twice recognises one behaviour rather than
/// two.
/// Names the standing yes as well as the two refusals. The person reading this
/// line has just answered a prompt, so they are the one for whom
/// `--auto-cookie` exists, and this sentence is the only place they would
/// learn it does. Naming only the ways to refuse taught half the control.
pub const AUTO_COOKIE_NOTICE: &str = "using the Roblox Studio cookie \
     (--auto-cookie to stop asking; --no-auto-cookie or RBX_COOKIE= to refuse)";

/// Print [`AUTO_COOKIE_NOTICE`] once per process.
///
/// Once, not per call: a single command can resolve the cookie two or three
/// times while building clients, and a notice repeated three times reads like
/// three separate reads of the credential. The same reasoning (and the same
/// shape) as `places::warn_unknown_keys`.
fn announce_auto_cookie() {
    static ANNOUNCED: std::sync::Once = std::sync::Once::new();
    ANNOUNCED.call_once(|| eprintln!("{AUTO_COOKIE_NOTICE}"));
}

/// What a run with nowhere to ask is told, once.
///
/// It names the two ways forward rather than the refusal alone: a CI job wants
/// `RBX_COOKIE` from a secret store, and a person who meant it wants
/// `--auto-cookie`. Saying only "not used" sends both to read the docs.
pub const AUTO_COOKIE_DECLINED: &str =
    "a Roblox Studio session is signed in on this machine and was not used: sending it needs \
     --auto-cookie, or set RBX_COOKIE to a value you chose. Nothing was sent.";

/// The question, asked once per process.
const AUTO_COOKIE_PROMPT: &str =
    "Roblox Studio is signed in on this machine. Send that session cookie? \
     It is a full-account credential, more powerful than any API key. [y/N] ";

/// Whether a Studio cookie found on this machine may actually be sent.
///
/// Three answers, in order. `--auto-cookie` is a standing yes. A run that can
/// hold a conversation asks, once, and the answer is remembered for the life of
/// the process, because a command that builds three clients would otherwise ask
/// three times for one decision. A run with nowhere to ask declines and says so.
///
/// "Can hold a conversation" means all three streams are terminals. stdin is
/// where the answer comes from and stderr is where the question is drawn, which
/// is [`crate::output::is_interactive`]; stdout is added here because a
/// redirected stdout is a run being captured (a `--json` document, a pipe into
/// `jq`) and a prompt in front of that is a pipeline stalling on a question
/// nobody sees.
fn auto_cookie_allowed(standing_yes: bool) -> bool {
    if standing_yes {
        return true;
    }
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ANSWER.get_or_init(|| {
        if !crate::output::is_interactive() || !std::io::stdout().is_terminal() {
            eprintln!("{AUTO_COOKIE_DECLINED}");
            return false;
        }
        match ask_yes_no(AUTO_COOKIE_PROMPT) {
            Some(true) => true,
            Some(false) | None => {
                eprintln!("{AUTO_COOKIE_DECLINED}");
                false
            }
        }
    })
}

/// Draw a yes/no question on stderr and read the answer from stdin.
///
/// stderr rather than stdout, because stdout may be carrying a document. Hand
/// rolled rather than `dialoguer`, which draws on stdout and would put the
/// question inside whatever the command is emitting.
///
/// `None` when the answer could not be read at all, which is treated as a no by
/// the caller: an unanswerable question is not consent.
#[cfg(not(test))]
fn ask_yes_no(question: &str) -> Option<bool> {
    use std::io::{BufRead, Write};

    eprint!("{question}");
    std::io::stderr().flush().ok()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).ok()?;
    let answer = answer.trim().to_ascii_lowercase();
    Some(answer == "y" || answer == "yes")
}

#[cfg(test)]
fn ask_yes_no(_question: &str) -> Option<bool> {
    // The unit tests never reach here: `auto_cookie_allowed` takes its
    // non-interactive branch first, since a test binary's streams are not
    // terminals. Present so the module compiles under `cfg(test)`.
    None
}

/// The Studio lookup, behind a seam so the unit tests can say what the machine
/// has.
///
/// Without it, "`--no-auto-cookie` stops the lookup" is only assertable on a
/// developer's machine with Studio signed in, and passes vacuously in CI, on
/// exactly the check that was broken. Same reasoning as the workspace's
/// `#[cfg(test)] with_base_url` seams: the production path is unchanged and
/// unconditional, the test build gets somewhere to stand.
///
/// `cfg(test)` covers this crate's unit tests only, so `tests/env.rs` keeps
/// exercising the real lookup.
#[cfg(not(test))]
fn studio_cookie() -> Option<String> {
    rbx_cookie::get_value()
}

#[cfg(test)]
fn studio_cookie() -> Option<String> {
    tests::studio_cookie_stub()
}

/// Cross-cutting CLI flags. Every flag here is `global = true` so it can be
/// placed either before or after the subcommand on the command line:
/// `rbx --env prod shop sync` and `rbx shop sync --env prod` both work.
#[derive(Args, Debug, Clone)]
pub struct GlobalFlags {
    /// Open Cloud API key.
    ///
    /// Required for most Open Cloud writes; some subcommands fall back to
    /// cookie auth. Defaults to the `RBX_API_KEY` env var.
    // `hide_env_values` is not cosmetic: without it clap prints
    // `[env: RBX_API_KEY=<the actual key>]` in every `--help`, so a key leaks
    // into any pasted output, CI log or screenshot of a help page.
    #[arg(long, env = "RBX_API_KEY", hide_env_values = true, global = true)]
    pub api_key: Option<String>,

    /// Roblox session cookie (`.ROBLOSECURITY`).
    ///
    /// Required for legacy endpoints not yet exposed by Open Cloud (group
    /// creation, some universe-config writes). Defaults to the `RBX_COOKIE`
    /// env var, then to Studio auto-detection unless `--no-auto-cookie` is
    /// set.
    #[arg(long, env = "RBX_COOKIE", hide_env_values = true, global = true)]
    pub cookie: Option<String>,

    /// Skip auto-detecting the cookie from a local Studio install.
    #[arg(long, global = true)]
    pub no_auto_cookie: bool,

    /// Use a locally signed-in Studio session without asking first.
    ///
    /// Auto-detection is opt-in: a `.ROBLOSECURITY` cookie is a full-account
    /// credential, so finding one on the machine is not the same as being
    /// allowed to send it. Without this flag an interactive run asks once and
    /// a non-interactive one refuses, which is what keeps CI from reaching
    /// into a developer's session.
    ///
    /// This is the standing "yes". `--no-auto-cookie` is the standing "no" and
    /// wins over it.
    #[arg(long, global = true, conflicts_with = "no_auto_cookie")]
    pub auto_cookie: bool,

    /// Target env from `rbxplace.toml` (or `all`).
    ///
    /// Pass `all` to act on every env defined there. Omitting `--env` falls
    /// back to per-subcommand standalone configuration (e.g. `[experience]`
    /// in `rbxshop.toml`).
    #[arg(long, short = 'e', global = true)]
    pub env: Option<String>,

    /// Place within the chosen env.
    ///
    /// Looks up `[<env>.places.<name>]`. Defaults to `main` when present,
    /// otherwise the only entry. Only consulted by subcommands that operate
    /// at place scope.
    #[arg(long, global = true)]
    pub place: Option<String>,

    /// Path to `rbxplace.toml`.
    ///
    /// Only used when `--env` is passed.
    #[arg(long, global = true, default_value = "rbxplace.toml")]
    pub places: PathBuf,

    /// Universe id, instead of naming an env.
    ///
    /// Skips `rbxplace.toml` entirely, so a command works in a directory that
    /// has none: a one-off against a universe you have not configured, or
    /// somebody else's game you are helping with.
    ///
    /// Wins over `--env` when both are given, because passing an id is the more
    /// specific instruction of the two.
    ///
    /// Spelled `--universe-id` to match the per-subcommand flags it replaced.
    /// Those were separate arguments with the same meaning, and because this
    /// one is `global = true` clap accepted `--universe` everywhere while
    /// `shop init` and friends read only their own: a flag with the right
    /// value, silently ignored. `--universe` stays as an alias so no existing
    /// invocation breaks.
    #[arg(long = "universe-id", alias = "universe", global = true)]
    pub universe_id: Option<u64>,

    /// Place id, instead of naming an env and a place.
    ///
    /// The place-scoped counterpart of `--universe-id`, and it exists for the
    /// same reason: a command should work in a directory with no
    /// `rbxplace.toml`, against a place you have not configured or somebody
    /// else's game you are helping with. Without it, everything at place scope
    /// was tied to the file even when a place id was all the call needed:
    /// `rbx open` reads the file, then builds a URI out of one number.
    ///
    /// Wins over `--env` / `--place` when both are given, because passing an id
    /// is the more specific instruction of the two.
    ///
    /// **It does not inherit `confirm = true`.** That guard lives on an env in
    /// `rbxplace.toml`, and naming a raw id resolves no env. Where the file is
    /// present and maps this id, the env's `confirm` is honoured anyway (see
    /// [`Self::confirm_for_place_id`]) so pointing at your own production
    /// place by id still prompts. An id the file does not know is a place
    /// outside the project and prompts on nothing.
    /// Repeatable, because `rbx apikey can-manage` answers about several
    /// places in one run and there must be exactly one spelling of "a place
    /// id" in the tool. Commands that act on one place call
    /// [`Self::single_place`], which refuses a repeated flag by name rather
    /// than silently taking the first.
    #[arg(long = "place-id", global = true)]
    pub place_id: Vec<u64>,
}

impl GlobalFlags {
    /// Resolve the cookie: every explicit source first, then Studio
    /// auto-detection (unless `--no-auto-cookie` is set).
    ///
    /// Auto-detection announces itself on stderr; the explicit paths do not.
    /// See [`AUTO_COOKIE_NOTICE`].
    ///
    /// **This is the only place the Studio lookup happens.** `rbx-apikey` used
    /// to run a second one in `resolve_cookie_from_env`, reached through an
    /// `.or_else` on this function's `None`, and that second site did not know
    /// about `--no-auto-cookie`. So the flag made this function return `None`
    /// and handed the decision to a function that auto-detected anyway: every
    /// `rbx apikey` subcommand read the session cookie with no working way to
    /// refuse. An escape hatch that silently does not work is worse than none,
    /// because the user believes they opted out.
    ///
    /// One chain is what keeps it from happening again. Two lookup sites is
    /// what made the divergence possible; one cannot diverge from itself. That
    /// is also why a caller wanting "the cookie this run will use" must call
    /// this rather than read [`Self::cookie`]: the flag is one source of
    /// several, and the two answers differ on exactly the machines where it
    /// matters.
    ///
    /// ## Auto-detection is opt-in
    ///
    /// Finding a signed-in Studio on the machine is not the same as being
    /// allowed to send its session. A `.ROBLOSECURITY` cookie is a full-account
    /// credential, not scoped to a universe and not revocable per tool, so a
    /// binary that reads one without asking is the first thing a
    /// security-minded reader looks for (#19).
    ///
    /// So the found value is a candidate until something says yes:
    /// `--auto-cookie` is the standing yes, and an interactive run is asked
    /// once and remembers the answer for the process. A run with nowhere to
    /// ask (CI, a pipe, a cron job) takes the safe branch and reports it,
    /// which is what keeps a pipeline from reaching into whoever's session the
    /// runner happens to have.
    pub fn resolve_cookie(&self) -> Option<String> {
        if let Some(c) = &self.cookie {
            return Some(c.clone());
        }
        if self.no_auto_cookie {
            return None;
        }
        let cookie = studio_cookie()?;
        if !auto_cookie_allowed(self.auto_cookie) {
            return None;
        }
        announce_auto_cookie();
        Some(cookie)
    }

    /// What `--env` names, resolved against `rbxplace.toml`.
    ///
    /// `None` when no `--env` was passed, which is the caller's domain config
    /// to answer (`[experience]` and friends), not this function's.
    ///
    /// Every plural selector goes through here, so a group is understood
    /// wherever `all` is and nowhere else has to know groups exist.
    pub fn env_selector(&self) -> Result<Option<EnvSelector>> {
        let Some(value) = self.env.as_deref() else {
            return Ok(None);
        };
        let places =
            PlacesFile::load(&self.places).with_context(|| format!("resolving env `{value}`"))?;
        Ok(Some(places.selector(value)?))
    }

    /// The universe to act on, from `--universe-id` or by resolving `--env`.
    ///
    /// For subcommands that act on exactly one universe and have no per-tool
    /// config to fall back on. A plural selector is rejected here rather than
    /// silently taking the first: a command written for one universe should say
    /// so rather than guess.
    pub fn single_universe(&self) -> Result<u64> {
        if let Some(universe) = self.universe_id {
            return Ok(universe);
        }
        let Some(value) = self.env.as_deref() else {
            bail!(
                "no target. Pass --env <name> to resolve one from rbxplace.toml, \
                 or --universe-id <id> to name it directly."
            );
        };
        // `all` is refused before the file is read, so the answer does not
        // depend on there being one. A group name cannot be recognised without
        // it, which is the one asymmetry the feature costs.
        if value == ALL_ENVS {
            EnvSelector::Every.single("universes")?;
        }
        let places =
            PlacesFile::load(&self.places).with_context(|| format!("resolving env `{value}`"))?;
        let selector = places.selector(value)?;
        let name = selector.single("universes")?;
        Ok(places.get(name)?.universe_id)
    }

    /// The envs to act on, expanded.
    ///
    /// `--env all` expands to every env in `rbxplace.toml`; `--env <group>` to
    /// that group's members, in declared order; `--env <name>` to that one env.
    /// No flag returns an empty Vec; the caller's domain-specific config (e.g.
    /// `[experience]`) handles the fallback.
    ///
    /// **A group is expanded here and never travels further.** Every lockfile in
    /// the suite keys on `(env name -> universe_id)`, and a group has no
    /// universe of its own, so a group name reaching one would invent an env.
    /// Expanding at the front is what makes that unrepresentable rather than
    /// merely avoided.
    ///
    /// Tools that only need `universe_id` (rbx shop, parts of rbx meta,
    /// rbx apikey) call this. Tools that also need `place_id` (rbx place,
    /// rbx meta place-level fields) layer [`Self::resolve_place`] on top.
    pub fn resolve_envs(&self) -> Result<Vec<EnvTarget>> {
        let Some(value) = self.env.as_deref() else {
            return Ok(Vec::new());
        };
        let places =
            PlacesFile::load(&self.places).with_context(|| format!("resolving env `{value}`"))?;
        let names = match places.selector(value)? {
            EnvSelector::One(name) => vec![name],
            EnvSelector::Every => places.env_names(),
            EnvSelector::Group { members, .. } => members,
        };
        if names.is_empty() {
            bail!(
                "rbxplace.toml has no envs defined. \
                 Add at least one [<env>] section with universe_id."
            );
        }
        names
            .into_iter()
            .map(|name| {
                let universe_id = places.get(&name)?.universe_id;
                Ok(EnvTarget { name, universe_id })
            })
            .collect()
    }

    /// Resolve a single env target. Errors on a plural selector (only one env
    /// makes sense for subcommands operating on one at a time, e.g.
    /// `rbx shop list`).
    pub fn resolve_single_env(&self) -> Result<Option<EnvTarget>> {
        let Some(value) = self.env.as_deref() else {
            return Ok(None);
        };
        if value == ALL_ENVS {
            EnvSelector::Every.single("envs")?;
        }
        let places =
            PlacesFile::load(&self.places).with_context(|| format!("resolving env `{value}`"))?;
        let name = places.selector(value)?.single("envs")?.to_string();
        let universe_id = places.get(&name)?.universe_id;
        Ok(Some(EnvTarget { name, universe_id }))
    }

    /// Resolve `(universe_id, place_id)` for the targeted env. Used by
    /// subcommands that need a specific place file (rbxplace, parts of
    /// rbxmeta). Honors `--place <name>` for envs with multiple places.
    pub fn resolve_place(&self, env: &str) -> Result<(u64, u64)> {
        places::resolve(&self.places, env, self.place.as_deref())
    }

    /// The place to act on, from `--place-id` or by resolving `--env` /
    /// `--place`.
    ///
    /// The place-scoped twin of [`Self::single_universe`]. A plural selector is
    /// rejected rather than silently taking the first: a command written for
    /// one place should say so rather than guess.
    pub fn single_place(&self) -> Result<u64> {
        match self.place_id.as_slice() {
            [one] => return Ok(*one),
            [] => {}
            several => bail!(
                "--place-id was given {} times; this command acts on one place. Name one, or run it once per place.",
                several.len()
            ),
        }
        let Some(value) = self.env.as_deref() else {
            bail!(
                "no target. Pass --env <name> to resolve one from rbxplace.toml, \
                 or --place-id <id> to name it directly."
            );
        };
        // Refused before the file is read, for the reason `single_universe`
        // documents.
        if value == ALL_ENVS {
            EnvSelector::Every.single("places")?;
        }
        let places =
            PlacesFile::load(&self.places).with_context(|| format!("resolving env `{value}`"))?;
        let selector = places.selector(value)?;
        let name = selector.single("places")?;
        Ok(self.resolve_place(name)?.1)
    }

    /// Whether a write to `--place-id` should still prompt.
    ///
    /// The `confirm = true` guard belongs to an env, and `--place-id` resolves
    /// none, so on its own it would walk straight past a guard the file
    /// author set on purpose. That is the wrong default for the case that
    /// actually happens: somebody points at their own production place by id
    /// because it was quicker than remembering the env name.
    ///
    /// So the id is looked up. If `rbxplace.toml` exists and some env maps this
    /// place, that env's `confirm` applies. If the file is absent, unreadable,
    /// or simply does not know this id, the answer is `false`: the place is
    /// outside the project and there is no declared intent to honour.
    ///
    /// Reading it back is deliberately best-effort. A missing or broken file is
    /// the ordinary case for `--place-id`, and failing the command over it
    /// would defeat the flag.
    pub fn confirm_for_place_id(&self, place_id: u64) -> bool {
        let Ok(places) = PlacesFile::load(&self.places) else {
            return false;
        };
        places.env_names().into_iter().any(|name| {
            places
                .get(&name)
                .map(|env| env.confirm() && env.places.values().any(|id| *id == place_id))
                .unwrap_or(false)
        })
    }
}

/// One env to operate on for a given command invocation. Place id is opaque
/// here; subcommands that need it call [`GlobalFlags::resolve_place`].
#[derive(Debug, Clone)]
pub struct EnvTarget {
    pub name: String,
    pub universe_id: u64,
}

#[cfg(test)]
// No `#[allow(unsafe_code)]` here any more. It was needed while these tests
// set the per-tool cookie variable retired in 0.9.0, since `set_var` is
// `unsafe` under the 2024 semantics; with it gone the whole module works off
// the thread-local Studio seam, and leaving the allow in place would silently
// permit the next `set_var` to come back. `tests/env.rs` still needs its own,
// for the variables clap reads.
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// What [`studio_cookie`] finds during a test. `None` is a machine with
        /// no Studio install; `Some` is one with a signed-in session.
        static STUDIO: Cell<Option<&'static str>> = const { Cell::new(None) };
    }

    pub(super) fn studio_cookie_stub() -> Option<String> {
        STUDIO.with(|s| s.get().map(str::to_string))
    }

    fn with_studio_cookie<T>(cookie: Option<&'static str>, body: impl FnOnce() -> T) -> T {
        STUDIO.with(|s| s.set(cookie));
        let out = body();
        STUDIO.with(|s| s.set(None));
        out
    }

    /// Default flags: nothing passed, so a found Studio cookie is a candidate
    /// and not yet a credential.
    fn flags() -> GlobalFlags {
        GlobalFlags {
            api_key: None,
            cookie: None,
            no_auto_cookie: false,
            auto_cookie: false,
            env: None,
            place: None,
            places: PathBuf::from("rbxplace.toml"),
            universe_id: None,
            place_id: Vec::new(),
        }
    }

    /// `--auto-cookie`: the standing yes. A test binary's streams are not
    /// terminals, so this is the only way to reach the auto-detected path from
    /// here, which is the behaviour under test, not a limitation of it.
    fn consented() -> GlobalFlags {
        GlobalFlags {
            auto_cookie: true,
            ..flags()
        }
    }

    fn no_auto() -> GlobalFlags {
        GlobalFlags {
            no_auto_cookie: true,
            auto_cookie: false,
            ..flags()
        }
    }

    /// The explicit source wins, and neither cookie flag can take it away.
    ///
    /// `--cookie` / `RBX_COOKIE` is the only explicit source left: it beats
    /// a signed-in Studio, and `--no-auto-cookie` does not suppress it, because
    /// that flag governs auto-detection and not what the user typed.
    #[test]
    fn cookie_resolution_order() {
        let mut explicit = flags();
        explicit.cookie = Some("from-flag".into());

        let mut explicit_no_auto = no_auto();
        explicit_no_auto.cookie = Some("from-flag".into());

        with_studio_cookie(Some("from-studio"), || {
            assert_eq!(
                explicit.resolve_cookie().as_deref(),
                Some("from-flag"),
                "an explicit cookie must beat one nobody asked for"
            );
            assert_eq!(
                explicit_no_auto.resolve_cookie().as_deref(),
                Some("from-flag"),
                "--no-auto-cookie governs auto-detection, not an explicit source"
            );
        });
    }

    /// #20. The flag has to stop the Studio lookup on a machine that has one to
    /// find, which is why the lookup is behind a seam: asserted against the
    /// real `rbx_cookie`, this would pass in CI whether or not the flag worked,
    /// on exactly the check that was broken.
    #[test]
    fn no_auto_cookie_stops_the_studio_lookup() {
        with_studio_cookie(Some("from-studio"), || {
            assert!(
                no_auto().resolve_cookie().is_none(),
                "--no-auto-cookie must not reach the Studio lookup"
            );
        });
    }

    /// #19. A signed-in Studio is a candidate, not a credential: without a yes
    /// from somewhere, the cookie found on the machine is not sent.
    ///
    /// The test binary's streams are not terminals, which is the same branch a
    /// CI runner takes, so this asserts the property that matters most: a
    /// pipeline cannot reach into whoever's session the runner happens to have.
    #[test]
    fn a_found_cookie_is_not_sent_without_consent() {
        with_studio_cookie(Some("from-studio"), || {
            assert!(
                flags().resolve_cookie().is_none(),
                "auto-detection is opt-in: nothing said yes"
            );
        });
    }

    /// And `--auto-cookie` is that yes, so the convenience stays one flag away
    /// rather than being removed.
    #[test]
    fn the_standing_yes_reaches_the_studio_cookie() {
        with_studio_cookie(Some("from-studio"), || {
            assert_eq!(consented().resolve_cookie().as_deref(), Some("from-studio"));
        });
    }

    /// The two flags are opposites, and the no wins: `--no-auto-cookie` is the
    /// one somebody sets in a profile to be safe everywhere, so a stray
    /// `--auto-cookie` in one invocation must not quietly undo it. clap refuses
    /// the pair outright, which is the version of this that cannot be argued
    /// with, and this pins the resolution side of it too.
    #[test]
    fn the_standing_no_beats_the_standing_yes() {
        let both = GlobalFlags {
            no_auto_cookie: true,
            auto_cookie: true,
            ..flags()
        };
        with_studio_cookie(Some("from-studio"), || {
            assert!(both.resolve_cookie().is_none());
        });
    }

    #[test]
    fn nothing_anywhere_resolves_to_nothing() {
        with_studio_cookie(None, || {
            assert!(consented().resolve_cookie().is_none());
        });
    }
}
