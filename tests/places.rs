#![allow(clippy::unwrap_used)]
use rbx_core::owner::OwnerType;
use rbx_core::places::{
    is_reserved_env_name, resolve, resolve_places_path, resolve_universe_id, unknown_keys,
    unknown_keys_warning, PlacesFile, PLACES_FILE, RESERVED_ENV_NAMES,
};
use tempfile::tempdir;

fn write_places(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rbxplace.toml");
    std::fs::write(&path, content).unwrap();
    (dir, path)
}

#[test]
fn loads_multi_env_file() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
[dev.places]
main = 1001

[prod]
universe_id = 200
[prod.places]
main = 2001
"#,
    );
    let places = PlacesFile::load(&path).unwrap();
    assert_eq!(places.env_names(), vec!["dev", "prod"]);
    assert_eq!(places.get("dev").unwrap().universe_id, 100);
    assert_eq!(places.get("prod").unwrap().universe_id, 200);
}

#[test]
fn an_unknown_env_field_still_loads_but_is_reported() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
confirm = true
some_unknown_field = "x"
"#,
    );
    let places = PlacesFile::load(&path).unwrap();
    // Loading must not fail: a key from a newer release has to stay readable,
    // or upgrading the file would mean upgrading every machine at once.
    assert_eq!(places.get("dev").unwrap().universe_id, 100);
    // But it must not be silent either — that is the whole defect.
    assert_eq!(places.unknown.len(), 1);
    assert_eq!(places.unknown[0].table, "dev");
    assert_eq!(places.unknown[0].key, "some_unknown_field");
}

#[test]
fn a_key_the_binary_predates_is_reported_the_same_as_a_typo() {
    // The reported case: `codegen` landed after v0.7.0, so a v0.7.0 binary
    // swallowed it and generated the env anyway. From the outside that was
    // indistinguishable from the key not existing. Simulated here with a name
    // no release will ever have, since this build does know `codegen`.
    let unknown = unknown_keys("[assets]\nuniverse_id = 1\ncodegen_from_the_future = false\n");
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].table, "assets");
    assert_eq!(unknown[0].key, "codegen_from_the_future");
    // The hint names what the table does take, so a typo is fixable on sight.
    assert!(unknown[0].known.contains(&"universe_id"));
}

#[test]
fn every_documented_env_key_is_accepted() {
    // The list is hand-maintained against three structs in three crates, so a
    // key that gains a field but not a list entry would warn on a correct
    // file — noise that gets the warning ignored.
    let unknown = unknown_keys(
        r#"
[owner]
type = "group"
id = 1

[codegen]
output = "src/shared/Envs.luau"

[prod]
universe_id = 200
confirm = true
env = "Production"
codegen = true
owner = { type = "user", id = 42 }
[prod.places]
main = 2001
lobby = 2002
"#,
    );
    assert!(unknown.is_empty(), "got {unknown:?}");
}

#[test]
fn place_names_are_data_and_never_reported_as_keys() {
    // `[<env>.places]` holds arbitrary names. Linting them would warn on
    // every real file.
    let unknown = unknown_keys("[dev]\nuniverse_id = 1\n[dev.places]\nwhatever_name = 5\n");
    assert!(unknown.is_empty(), "got {unknown:?}");
}

#[test]
fn the_reserved_tables_are_checked_against_their_own_keys() {
    let unknown = unknown_keys(
        r#"
[owner]
type = "group"
id = 1
nickname = "x"

[codegen]
outputs = "typo.luau"

[dev]
universe_id = 1
owner = { type = "user", id = 42, note = "inline" }
"#,
    );
    let found: Vec<(&str, &str)> = unknown
        .iter()
        .map(|k| (k.table.as_str(), k.key.as_str()))
        .collect();
    assert_eq!(
        found,
        vec![
            ("codegen", "outputs"),
            ("dev.owner", "note"),
            ("owner", "nickname"),
        ]
    );
}

#[test]
fn a_file_that_does_not_parse_reports_nothing() {
    // The typed parse owns that error, and reports it with a line number the
    // lint cannot produce. Two complaints about one broken file is worse.
    assert!(unknown_keys("[dev\nuniverse_id =").is_empty());
}

#[test]
fn nothing_unknown_produces_no_warning() {
    assert!(unknown_keys_warning(std::path::Path::new("rbxplace.toml"), &[]).is_none());
}

#[test]
fn the_warning_names_the_key_the_table_and_what_the_table_accepts() {
    // The wording is the whole point of the warning — an ignored key looks
    // exactly like an honoured one from the outside — and it used to be
    // unreachable from a test, sitting behind a process-global "warn once"
    // set that the first test to touch a path would mute for every later one.
    let unknown = unknown_keys("[prod]\nuniverse_id = 1\nconfrm = true\n");
    let warning = unknown_keys_warning(std::path::Path::new("rbxplace.toml"), &unknown)
        .expect("a misspelled key should warn");

    assert!(warning.contains("rbxplace.toml"), "got: {warning}");
    assert!(warning.contains("confrm"), "got: {warning}");
    assert!(warning.contains("[prod]"), "got: {warning}");
    assert!(warning.contains("universe_id"), "known keys: {warning}");
    assert!(
        warning.contains("1 unrecognised key,"),
        "singular, not 'key(s)': {warning}"
    );
    assert!(
        warning.contains(env!("CARGO_PKG_VERSION")),
        "the version is what makes 'from a newer release' checkable: {warning}"
    );
}

#[test]
fn resolve_universe_id_only() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
"#,
    );
    assert_eq!(resolve_universe_id(&path, "dev").unwrap(), 100);
}

#[test]
fn resolve_universe_and_place_with_main_default() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
[dev.places]
main = 1001
lobby = 1002
"#,
    );
    let (u, p) = resolve(&path, "dev", None).unwrap();
    assert_eq!(u, 100);
    assert_eq!(p, 1001);
}

#[test]
fn resolve_picks_single_place_when_no_main() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
[dev.places]
solo = 999
"#,
    );
    let (u, p) = resolve(&path, "dev", None).unwrap();
    assert_eq!(u, 100);
    assert_eq!(p, 999);
}

#[test]
fn resolve_errors_on_ambiguous_places() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
[dev.places]
a = 1
b = 2
"#,
    );
    let err = resolve(&path, "dev", None).unwrap_err().to_string();
    assert!(err.contains("multiple places"));
    assert!(err.contains("a, b"));
}

#[test]
fn resolve_respects_explicit_place_override() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
[dev.places]
main = 1
lobby = 2
"#,
    );
    let (_, p) = resolve(&path, "dev", Some("lobby")).unwrap();
    assert_eq!(p, 2);
}

#[test]
fn resolve_errors_on_unknown_env_with_available_list() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
[prod]
universe_id = 200
"#,
    );
    let err = resolve_universe_id(&path, "staging")
        .unwrap_err()
        .to_string();
    assert!(err.contains("'staging' not found"));
    assert!(err.contains("dev"));
    assert!(err.contains("prod"));
}

#[test]
fn confirm_defaults_to_false_when_absent() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
"#,
    );
    let places = PlacesFile::load(&path).unwrap();
    assert!(!places.get("dev").unwrap().confirm());
}

#[test]
fn confirm_returns_true_when_set() {
    let (_d, path) = write_places(
        r#"
[prod]
universe_id = 200
confirm = true
"#,
    );
    let places = PlacesFile::load(&path).unwrap();
    assert!(places.get("prod").unwrap().confirm());
}

#[test]
fn a_confirm_that_is_not_a_bool_fails_the_load_instead_of_reading_as_false() {
    // The reason `confirm` is a typed field and not a string read out of the
    // `_extra` map: quoting it is the mistake a YAML habit produces, and the
    // old code answered it by silently disabling the prompt — on the env most
    // likely to be prod.
    let (_d, path) = write_places(
        r#"
[prod]
universe_id = 200
confirm = "true"
"#,
    );
    let err = PlacesFile::load(&path).unwrap_err().to_string();
    assert!(err.contains("Failed to parse"), "got: {err}");
}

#[test]
fn resolve_errors_when_no_places_section() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
"#,
    );
    let err = resolve(&path, "dev", None).unwrap_err().to_string();
    assert!(err.contains("no [<env>.places]"));
}

#[test]
fn places_without_owner_block_returns_none() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
"#,
    );
    let places = PlacesFile::load(&path).unwrap();
    assert!(places.owner.is_none());
    assert!(places.resolve_owner("dev").is_none());
    assert!(places.resolve_owner("missing").is_none());
}

#[test]
fn parses_top_level_owner_block() {
    let (_d, path) = write_places(
        r#"
[owner]
type = "group"
id = 1234567

[dev]
universe_id = 100
"#,
    );
    let places = PlacesFile::load(&path).unwrap();
    let owner = places.owner.as_ref().unwrap();
    assert_eq!(owner.kind, OwnerType::Group);
    assert_eq!(owner.id, 1234567);
    // env names must not include the reserved `owner` key.
    assert_eq!(places.env_names(), vec!["dev"]);
}

#[test]
fn resolve_owner_falls_back_to_top_level() {
    let (_d, path) = write_places(
        r#"
[owner]
type = "group"
id = 7

[dev]
universe_id = 100

[prod]
universe_id = 200
"#,
    );
    let places = PlacesFile::load(&path).unwrap();
    let dev_owner = places.resolve_owner("dev").unwrap();
    assert_eq!(dev_owner.kind, OwnerType::Group);
    assert_eq!(dev_owner.id, 7);
    // Same for prod, same id.
    let prod_owner = places.resolve_owner("prod").unwrap();
    assert_eq!(prod_owner.id, 7);
}

#[test]
fn per_env_owner_overrides_top_level() {
    let (_d, path) = write_places(
        r#"
[owner]
type = "group"
id = 7

[dev]
universe_id = 100
owner = { type = "user", id = 42 }

[prod]
universe_id = 200
"#,
    );
    let places = PlacesFile::load(&path).unwrap();
    let dev_owner = places.resolve_owner("dev").unwrap();
    assert_eq!(dev_owner.kind, OwnerType::User);
    assert_eq!(dev_owner.id, 42);
    // prod still inherits.
    let prod_owner = places.resolve_owner("prod").unwrap();
    assert_eq!(prod_owner.kind, OwnerType::Group);
    assert_eq!(prod_owner.id, 7);
}

#[test]
fn per_env_owner_works_without_top_level() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
owner = { type = "user", id = 99 }

[prod]
universe_id = 200
"#,
    );
    let places = PlacesFile::load(&path).unwrap();
    assert!(places.owner.is_none());
    let dev_owner = places.resolve_owner("dev").unwrap();
    assert_eq!(dev_owner.kind, OwnerType::User);
    assert_eq!(dev_owner.id, 99);
    // prod has no owner anywhere.
    assert!(places.resolve_owner("prod").is_none());
}

#[test]
fn codegen_is_a_reserved_section_not_an_env() {
    // Without `codegen` declared as a known key, serde's flattened map would
    // try to read this table as an env and fail on the missing universe_id —
    // breaking every command that touches rbxplace.toml, not just gen-module.
    let (_d, path) = write_places(
        r#"
[codegen]
output = "src/shared/Envs.luau"

[dev]
universe_id = 100
"#,
    );
    let places = PlacesFile::load(&path).unwrap();
    assert_eq!(places.env_names(), vec!["dev".to_string()]);
    assert_eq!(
        places.codegen.unwrap().output.unwrap(),
        std::path::PathBuf::from("src/shared/Envs.luau")
    );
}

#[test]
fn a_file_without_codegen_still_loads() {
    let (_d, path) = write_places("[dev]\nuniverse_id = 100\n");
    let places = PlacesFile::load(&path).unwrap();
    assert!(places.codegen.is_none());
}

#[test]
fn two_sections_renaming_onto_the_same_env_are_rejected() {
    // Not an alias: two envs with one name. Any lookup by name silently picks
    // one of the two, and the generated module lists both as "dev".
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100

[staging]
universe_id = 555
env = "dev"
"#,
    );
    let err = PlacesFile::load(&path).unwrap_err().to_string();
    assert!(err.contains("resolve to the name 'dev'"), "got: {err}");
    assert!(err.contains("[dev]"), "got: {err}");
    assert!(err.contains("[staging]"), "got: {err}");
}

#[test]
fn two_env_overrides_colliding_with_each_other_are_rejected() {
    let (_d, path) = write_places(
        r#"
[a]
universe_id = 100
env = "shared"

[b]
universe_id = 200
env = "shared"
"#,
    );
    let err = PlacesFile::load(&path).unwrap_err().to_string();
    assert!(err.contains("resolve to the name 'shared'"), "got: {err}");
}

#[test]
fn renaming_to_a_name_nothing_else_uses_is_fine() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100
env = "Dev"

[prod]
universe_id = 200
"#,
    );
    let places = PlacesFile::load(&path).unwrap();
    assert_eq!(places.get("dev").unwrap().env.as_deref(), Some("Dev"));
}

#[test]
fn codegen_false_hides_an_env_from_generation_only() {
    let (_d, path) = write_places(
        r#"
[dev]
universe_id = 100

[ci]
universe_id = 555
codegen = false
"#,
    );
    let places = PlacesFile::load(&path).unwrap();

    // Still a normal env: --env resolves it, --env all includes it.
    assert_eq!(
        places.env_names(),
        vec!["ci".to_string(), "dev".to_string()]
    );
    assert_eq!(places.get("ci").unwrap().universe_id, 555);

    // Only generation filters it out.
    assert_eq!(places.codegen_env_names(), vec!["dev".to_string()]);
}

#[test]
fn envs_are_visible_to_game_code_unless_they_opt_out() {
    let (_d, path) = write_places("[dev]\nuniverse_id = 100\n[prod]\nuniverse_id = 200\n");
    let places = PlacesFile::load(&path).unwrap();
    assert_eq!(places.codegen_env_names(), places.env_names());
}

/// The writers (rbx-init, rbx-import) refuse these names, and this is the one
/// list they both read. It has to cover exactly the names this file already
/// spells for itself: `all` for "every env", plus the two reserved tables.
#[test]
fn the_reserved_env_names_are_the_ones_the_loader_never_returns() {
    assert!(is_reserved_env_name("all"));
    assert!(is_reserved_env_name("owner"));
    assert!(is_reserved_env_name("codegen"));
    assert!(!is_reserved_env_name("prod"));

    let (_d, path) = write_places(
        r#"
[owner]
type = "user"
id = 42

[codegen]
output = "src/shared/Envs.luau"

[prod]
universe_id = 100
"#,
    );
    let places = PlacesFile::load(&path).unwrap();
    for name in RESERVED_ENV_NAMES {
        assert!(
            !places.env_names().contains(&(*name).to_string()),
            "`{name}` came back as an env"
        );
    }
}

// ---------------------------------------------------------------------------
// --dir / --places
// ---------------------------------------------------------------------------

/// The default `--places` means "next to everything else this command reads
/// and writes", not "in the process's working directory". One rule, so `rbx
/// import --dir` and `rbx check --dir` cannot disagree about the same
/// directory.
#[test]
fn the_default_places_path_follows_dir() {
    let dir = std::path::Path::new("game");
    assert_eq!(
        resolve_places_path(std::path::Path::new(PLACES_FILE), dir),
        dir.join(PLACES_FILE)
    );
}

/// An explicit `--places` is a shared env file that may well live outside the
/// directory, so it wins.
#[test]
fn an_explicit_places_path_wins_over_dir() {
    assert_eq!(
        resolve_places_path(
            std::path::Path::new("shared/envs.toml"),
            std::path::Path::new("game")
        ),
        std::path::Path::new("shared/envs.toml")
    );
}

/// The default `--dir` is `.`, so the default pair still resolves to the file
/// in the working directory.
#[test]
fn the_default_pair_resolves_to_the_working_directory() {
    let dir = std::path::Path::new(".");
    assert_eq!(
        resolve_places_path(std::path::Path::new(PLACES_FILE), dir),
        dir.join(PLACES_FILE)
    );
}
