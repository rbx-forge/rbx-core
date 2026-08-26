#![allow(clippy::unwrap_used, unsafe_code)]
use std::path::PathBuf;

use clap::Parser;
use rbx_core::GlobalFlags;

#[derive(Parser)]
struct TestCli {
    #[command(flatten)]
    global: GlobalFlags,
}

fn parse(args: &[&str]) -> TestCli {
    let mut full = vec!["test"];
    full.extend(args);
    TestCli::try_parse_from(full).expect("parse")
}

/// Env-var assertions live in a single test so they don't race with parallel
/// tests that also mutate process-global env state.
#[test]
fn defaults_and_env_vars() {
    // SAFETY: only this test mutates RBX_* env vars; other tests don't touch them.
    unsafe { std::env::remove_var("RBX_API_KEY") };
    unsafe { std::env::remove_var("RBX_COOKIE") };
    // `resolve_cookie` reads this one too since #20 folded rbx-apikey's second
    // lookup into it. Left set, it is an explicit source, and the
    // `--no-auto-cookie` assertion below would fail on the machine of anyone
    // who happens to have it exported.
    unsafe { std::env::remove_var("RBXAPIKEY_COOKIE") };

    // Defaults when nothing is set.
    let cli = parse(&[]);
    assert!(cli.global.api_key.is_none());
    assert!(cli.global.cookie.is_none());
    assert!(!cli.global.no_auto_cookie);
    assert!(cli.global.env.is_none());
    assert!(cli.global.place.is_none());
    assert_eq!(cli.global.places, PathBuf::from("rbxplace.toml"));

    // Flag wins when explicitly passed.
    let cli = parse(&["--api-key", "test-key"]);
    assert_eq!(cli.global.api_key.as_deref(), Some("test-key"));

    // Env var picked up when no flag.
    unsafe { std::env::set_var("RBX_API_KEY", "env-key") };
    let cli = parse(&[]);
    assert_eq!(cli.global.api_key.as_deref(), Some("env-key"));

    // Flag overrides env var.
    let cli = parse(&["--api-key", "flag-key"]);
    assert_eq!(cli.global.api_key.as_deref(), Some("flag-key"));

    // The two escape hatches the auto-detection notice promises. Both have to
    // stop `resolve_cookie` before it reaches the Studio lookup, or the
    // sentence points people at controls that do nothing.
    unsafe { std::env::set_var("RBX_COOKIE", "") };
    let cli = parse(&[]);
    assert_eq!(
        cli.global.resolve_cookie().as_deref(),
        Some(""),
        "an empty RBX_COOKIE is an explicit answer, not an unset variable"
    );
    unsafe { std::env::remove_var("RBX_COOKIE") };

    let cli = parse(&["--no-auto-cookie"]);
    assert!(
        cli.global.resolve_cookie().is_none(),
        "--no-auto-cookie must skip the Studio lookup entirely"
    );

    // Cleanup.
    unsafe { std::env::remove_var("RBX_API_KEY") };
}

#[test]
fn the_auto_cookie_notice_names_both_ways_out() {
    // The wording is a contract in two directions: rbx-apikey prints this same
    // string at its own fallback site, and the sentence is the only place a
    // user is told the behaviour exists at all. If it stops naming a control,
    // the announcement stops being actionable and becomes noise.
    let notice = rbx_core::env::AUTO_COOKIE_NOTICE;
    assert!(notice.contains("--no-auto-cookie"), "got: {notice}");
    assert!(notice.contains("RBX_COOKIE="), "got: {notice}");
    assert!(notice.contains("Roblox Studio cookie"), "got: {notice}");
    // The yes, not only the two noes. Auto-detection is opt-in, so this line
    // is printed to somebody who has just been asked and said yes; without
    // `--auto-cookie` in it they are told how to refuse next time and never
    // that they can stop being asked. The test named itself after this and
    // then checked two spellings of the same direction.
    assert!(notice.contains("--auto-cookie"), "got: {notice}");
}

#[test]
fn env_short_flag() {
    let cli = parse(&["-e", "prod"]);
    assert_eq!(cli.global.env.as_deref(), Some("prod"));
}

#[test]
fn no_env_returns_empty_targets() {
    let cli = parse(&[]);
    let targets = cli.global.resolve_envs().unwrap();
    assert!(targets.is_empty());
}

/// One wording for every plural selector, so `all` and a group are refused the
/// same way. Four sites used to spell this out separately, two of them
/// differing only in punctuation.
#[test]
fn a_plural_selector_errors_for_the_single_env_helper() {
    let dir = tempfile::tempdir().unwrap();
    let places = dir.path().join("rbxplace.toml");
    std::fs::write(
        &places,
        "[groups]\nnonprod = [\"dev\"]\n\n[dev]\nuniverse_id = 100\n",
    )
    .unwrap();

    let cli = parse(&["--env", "all", "--places", places.to_str().unwrap()]);
    let err = cli.global.resolve_single_env().unwrap_err().to_string();
    assert!(err.contains("names several envs"), "got: {err}");

    // And a group, which used to fall through every one of those checks into
    // `resolve_universe_id` and fail as an env that does not exist.
    let cli = parse(&["--env", "nonprod", "--places", places.to_str().unwrap()]);
    let err = cli.global.resolve_single_env().unwrap_err().to_string();
    assert!(err.contains("is a group of 1 envs"), "got: {err}");
    assert!(err.contains("Name one of them"), "got: {err}");
}

#[test]
fn a_group_expands_to_its_members_in_declared_order() {
    let dir = tempfile::tempdir().unwrap();
    let places = dir.path().join("rbxplace.toml");
    std::fs::write(
        &places,
        "[groups]\nnonprod = [\"staging\", \"dev\"]\n\n\
         [dev]\nuniverse_id = 100\n[staging]\nuniverse_id = 150\n[prod]\nuniverse_id = 200\n",
    )
    .unwrap();

    let cli = parse(&["--env", "nonprod", "--places", places.to_str().unwrap()]);
    let targets = cli.global.resolve_envs().unwrap();
    let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
    // Declared order, not sorted: `--env all` sorts because it has no author
    // order to respect, and a group has one.
    assert_eq!(names, vec!["staging", "dev"]);
    assert_eq!(targets[0].universe_id, 150);
    assert_eq!(targets[1].universe_id, 100);

    // `prod` is not in the group, which is the entire point.
    assert!(!names.contains(&"prod"));
}

/// A group names several universes and several places, so both single-target
/// helpers must refuse it exactly as they refuse `all`.
#[test]
fn a_group_is_refused_by_both_single_target_helpers() {
    let dir = tempfile::tempdir().unwrap();
    let places = dir.path().join("rbxplace.toml");
    std::fs::write(
        &places,
        "[groups]\nnonprod = [\"dev\", \"staging\"]\n\n\
         [dev]\nuniverse_id = 100\n[dev.places]\nmain = 111\n\
         [staging]\nuniverse_id = 150\n[staging.places]\nmain = 222\n",
    )
    .unwrap();

    let cli = parse(&["--env", "nonprod", "--places", places.to_str().unwrap()]);
    let err = cli.global.single_universe().unwrap_err().to_string();
    assert!(err.contains("acts on one universe"), "got: {err}");

    let err = cli.global.single_place().unwrap_err().to_string();
    assert!(err.contains("acts on one place"), "got: {err}");

    // A member of the group still resolves on its own.
    let cli = parse(&["--env", "dev", "--places", places.to_str().unwrap()]);
    assert_eq!(cli.global.single_universe().unwrap(), 100);
    assert_eq!(cli.global.single_place().unwrap(), 111);
}

#[test]
fn env_all_expands_to_every_env() {
    let dir = tempfile::tempdir().unwrap();
    let places = dir.path().join("rbxplace.toml");
    std::fs::write(
        &places,
        "[dev]\nuniverse_id = 100\n[prod]\nuniverse_id = 200\n",
    )
    .unwrap();

    let cli = parse(&["--env", "all", "--places", places.to_str().unwrap()]);
    let targets = cli.global.resolve_envs().unwrap();
    assert_eq!(targets.len(), 2);
    let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["dev", "prod"]);
    assert_eq!(targets[0].universe_id, 100);
    assert_eq!(targets[1].universe_id, 200);
}

#[test]
fn resolve_place_uses_main_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let places = dir.path().join("rbxplace.toml");
    std::fs::write(
        &places,
        "[dev]\nuniverse_id = 100\n[dev.places]\nmain = 1001\nlobby = 1002\n",
    )
    .unwrap();

    let cli = parse(&["--env", "dev", "--places", places.to_str().unwrap()]);
    let (u, p) = cli.global.resolve_place("dev").unwrap();
    assert_eq!(u, 100);
    assert_eq!(p, 1001);
}

/// #19, asserted against the real `rbx_cookie` rather than the unit tests'
/// stub: on a machine with a signed-in Studio, a run that nobody said yes to
/// must not send the session.
///
/// A test binary's streams are not terminals, which is the same branch a CI
/// runner takes, so this is the property that matters most: a pipeline cannot
/// reach into whoever's session the runner happens to have. On a machine with
/// no Studio it passes for the boring reason, which is why the unit tests hold
/// the other half behind a seam.
#[test]
fn a_studio_session_is_not_sent_to_a_run_that_cannot_be_asked() {
    // SAFETY: the env-var tests are serialised into `defaults_and_env_vars`;
    // this one only clears, and clearing twice is harmless.
    unsafe { std::env::remove_var("RBX_COOKIE") };
    unsafe { std::env::remove_var("RBXAPIKEY_COOKIE") };

    let cli = parse(&[]);
    assert!(
        cli.global.resolve_cookie().is_none(),
        "auto-detection is opt-in, and there is nowhere here to ask"
    );
}

/// The refusal has to name both ways forward. A CI job wants `RBX_COOKIE` from
/// a secret store and a person who meant it wants `--auto-cookie`; a message
/// that says only "not used" sends both to read the docs.
#[test]
fn the_declined_notice_names_both_ways_forward() {
    let notice = rbx_core::env::AUTO_COOKIE_DECLINED;
    assert!(notice.contains("--auto-cookie"), "got: {notice}");
    assert!(notice.contains("RBX_COOKIE"), "got: {notice}");
    assert!(notice.contains("Nothing was sent"), "got: {notice}");
}

/// `--place-id` is the place-scoped twin of `--universe-id`, and the point of
/// both is working where there is no `rbxplace.toml` to resolve through.
#[test]
fn place_id_resolves_without_a_places_file() {
    let cli = parse(&[
        "--place-id",
        "123456789",
        "--places",
        "definitely/not/here.toml",
    ]);
    assert_eq!(cli.global.single_place().unwrap(), 123456789);
}

/// An id beats a name, the same way `--universe-id` beats `--env`: passing an
/// id is the more specific instruction of the two.
#[test]
fn place_id_wins_over_env_and_place() {
    let dir = tempfile::tempdir().unwrap();
    let places = dir.path().join("rbxplace.toml");
    std::fs::write(
        &places,
        "[dev]
universe_id = 100
[dev.places]
main = 1001
",
    )
    .unwrap();

    let cli = parse(&[
        "--env",
        "dev",
        "--place-id",
        "999",
        "--places",
        places.to_str().unwrap(),
    ]);
    assert_eq!(cli.global.single_place().unwrap(), 999);
}

/// With neither, the error names both ways to give one rather than only the
/// env, which is the half a reader outside a project cannot use.
#[test]
fn no_place_target_names_both_ways_to_give_one() {
    let cli = parse(&[]);
    let err = cli.global.single_place().unwrap_err().to_string();
    assert!(err.contains("--env"), "got: {err}");
    assert!(err.contains("--place-id"), "got: {err}");
}

/// `--env all` names several places; a command that acts on one must say so.
#[test]
fn env_all_is_refused_for_a_single_place() {
    let cli = parse(&["--env", "all"]);
    let err = cli.global.single_place().unwrap_err().to_string();
    assert!(err.contains("acts on one"), "got: {err}");
}

/// The confirm guard belongs to an env, and `--place-id` resolves none. Where
/// the file happens to map the id, the env's `confirm` still applies: pointing
/// at your own production place by id must not walk past a guard somebody set
/// on purpose.
#[test]
fn a_place_id_inherits_confirm_from_the_env_that_declares_it() {
    let dir = tempfile::tempdir().unwrap();
    let places = dir.path().join("rbxplace.toml");
    std::fs::write(
        &places,
        "[dev]
universe_id = 100
[dev.places]
main = 1001

         [prod]
universe_id = 200
confirm = true
[prod.places]
main = 2001
",
    )
    .unwrap();
    let cli = parse(&["--places", places.to_str().unwrap()]);

    assert!(cli.global.confirm_for_place_id(2001), "prod has confirm");
    assert!(!cli.global.confirm_for_place_id(1001), "dev does not");
    // A place the file has never heard of is outside the project, so there is
    // no declared intent to honour.
    assert!(!cli.global.confirm_for_place_id(4242), "unknown id");
}

/// A missing file is the ordinary case for `--place-id`, so the lookup answers
/// false rather than failing the command.
#[test]
fn confirm_lookup_is_best_effort_when_there_is_no_file() {
    let cli = parse(&["--places", "definitely/not/here.toml"]);
    assert!(!cli.global.confirm_for_place_id(2001));
}

/// Repeatable, for `rbx apikey can-manage`, which answers about several places
/// in one run. Everything else refuses it by name rather than taking the first.
#[test]
fn a_repeated_place_id_is_refused_by_single_place() {
    let cli = parse(&["--place-id", "1", "--place-id", "2"]);
    assert_eq!(cli.global.place_id, vec![1, 2]);
    let err = cli.global.single_place().unwrap_err().to_string();
    assert!(err.contains("given 2 times"), "got: {err}");
    assert!(err.contains("acts on one place"), "got: {err}");
}
