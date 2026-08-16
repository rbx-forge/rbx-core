//! Hints must name a command the workspace actually builds.
//!
//! The suite used to be a set of standalone binaries: `rbxapikey`, `rbxconfig`,
//! `rbxshop`, `rbxmeta`. They were folded into `rbx <subcommand>` before this
//! repository existed, but about fifty user-facing strings kept telling people
//! to run the old ones (#84) — remedy lines in `status`, usage lines in
//! `bail!`, and the header comments of the files `init` writes into the user's
//! repository. Every one of them names a command that fails with "not found".
//!
//! That failure is invisible from the inside. The reader is already stuck, does
//! exactly what the tool said, gets a second error unrelated to their problem,
//! and concludes the install is broken. A hint that is wrong is worse than no
//! hint at all.
//!
//! # What the matcher recognises
//!
//! A retired binary name followed by a space and a lowercase letter — the shape
//! of `rbxapikey status`, `rbxconfig pull --env <name>`, `rbxshop sync`. That is
//! how a command reads, and nothing else in this tree reads that way.
//!
//! The names are derived from the crate list rather than hard-coded, so a crate
//! added tomorrow is guarded the day it lands: `crates/rbx-<tool>` retires
//! `rbx<tool>`.
//!
//! # What it deliberately does not match
//!
//! `rbxapikey.toml`, `rbxshop.lock.toml`, `rbxplace.schema.json` and friends are
//! the real names of real files, written a few hundred times across this tree.
//! A check that flagged those would be turned off within the week, so the space
//! is load-bearing: the name must be followed by one, and then by the first
//! letter of a subcommand. `otherproject_rbxshop` (an API key name), `rbxshop:` (the
//! prefix on generated Luau errors) and `rbxshop/rbxmeta` in prose are out for
//! the same reason.

use std::fs;
use std::path::{Path, PathBuf};

/// This file's own name, skipped during the walk — it spells the retired names
/// out in its docs and its fixtures, which is the one place they belong.
const SELF: &str = "binary_names.rs";

/// The two binaries the workspace ships. `crates/rbx` and `crates/rbx-ops`
/// carry no retired per-tool name, so they contribute nothing to match on.
const SHIPPED: &[&str] = &["rbx", "rbx-ops"];

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/crates/rbx-core.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("rbx-core should live two levels below the workspace root")
        .to_path_buf()
}

/// `crates/rbx-apikey` → `rbxapikey`, for every crate but the shipped two.
fn retired_names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root.join("crates"))
        .expect("crates/ exists")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().join("Cargo.toml").is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|krate| !SHIPPED.contains(&krate.as_str()))
        .filter_map(|krate| krate.strip_prefix("rbx-").map(|tool| format!("rbx{tool}")))
        .collect();
    names.sort();
    names
}

/// Files whose text is checked: Rust sources under `crates/`, and the Markdown
/// under `docs/`. `CHANGELOG.md` is left out on purpose — it records what the
/// commands used to be called, which is history, not instruction.
fn scanned_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect(&root.join("crates"), "rs", &mut out);
    collect(&root.join("docs"), "md", &mut out);
    let readme = root.join("README.md");
    if readme.is_file() {
        out.push(readme);
    }
    out
}

fn collect(dir: &Path, extension: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `target` is build output, not source anyone reads.
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            collect(&path, extension, out);
        } else if path.extension().is_some_and(|ext| ext == extension) {
            out.push(path);
        }
    }
}

/// Every retired binary name in `text` that is written the way a command is:
/// the name, a space, then the first letter of a subcommand.
fn commands_that_do_not_exist(text: &str, retired: &[String]) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    for name in retired {
        for (at, _) in text.match_indices(name.as_str()) {
            // `otherproject_rbxshop`, `gen-rbxplace`: part of a longer word, not a
            // command anybody types on its own.
            let preceded = at
                .checked_sub(1)
                .map(|i| bytes[i] as char)
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if preceded {
                continue;
            }
            let rest = &text[at + name.len()..];
            if rest
                .strip_prefix(' ')
                .is_some_and(|after| after.starts_with(|c: char| c.is_ascii_lowercase()))
            {
                // Report the name plus the word after it, so the failure reads
                // as the command it is.
                let subcommand: String = rest[1..]
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                    .collect();
                found.push(format!("{name} {subcommand}"));
            }
        }
    }
    found
}

#[test]
fn no_user_facing_string_names_a_binary_the_workspace_does_not_build() {
    let root = workspace_root();
    let retired = retired_names(&root);
    let files = scanned_files(&root);
    assert!(
        !files.is_empty(),
        "found nothing to scan under {}; the path walk is wrong, not the code",
        root.display()
    );

    let mut offenders = Vec::new();
    for path in &files {
        if path.file_name().is_some_and(|name| name == SELF) {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            for command in commands_that_do_not_exist(line, &retired) {
                let relative = path.strip_prefix(&root).unwrap_or(path);
                offenders.push(format!(
                    "{}:{}: {command}\n    {}",
                    relative.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these tell the reader to run a binary that does not exist. The workspace ships two, \
         `rbx` and `rbx-ops`; the per-tool binaries were folded into subcommands. Write \
         `rbx apikey status`, not `rbxapikey status`. File names are not affected — \
         `rbxapikey.toml` and its siblings are matched only when followed by a space and a \
         subcommand.\n\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_matcher_finds_a_command_and_spares_the_file_names() {
    let retired = vec!["rbxapikey".to_string(), "rbxshop".to_string()];
    assert_eq!(
        commands_that_do_not_exist(
            r#".note("Tip: run `rbxapikey status --remote` to also check Roblox.")"#,
            &retired
        ),
        vec!["rbxapikey status"]
    );
    assert_eq!(
        commands_that_do_not_exist(
            "# Pull the live config: `rbxshop pull --env <name>`",
            &retired
        ),
        vec!["rbxshop pull"]
    );
    // File names, key names, and the tool's name used as a label all stay.
    assert!(commands_that_do_not_exist(
        "re-apply key config from rbxapikey.toml, next to rbxapikey.lock.toml",
        &retired
    )
    .is_empty());
    assert!(commands_that_do_not_exist(
        r#"remote_key("otherproject_rbxshop", Tracked::No)"#,
        &retired
    )
    .is_empty());
    assert!(
        commands_that_do_not_exist(r#"error(`rbxshop: unknown universe`)"#, &retired).is_empty()
    );
    // The fixed form must not be flagged by the very check that asked for it.
    assert!(commands_that_do_not_exist("run `rbx apikey status --remote`", &retired).is_empty());
}

/// The names are derived, so a broken derivation would silently guard nothing.
#[test]
fn the_retired_names_cover_the_tools_that_had_their_own_binary() {
    let names = retired_names(&workspace_root());
    for expected in ["rbxapikey", "rbxconfig", "rbxshop", "rbxmeta", "rbxplace"] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} is missing from {names:?}; the crate list or the rbx- prefix stripping \
             changed, and the check above now guards less than it did"
        );
    }
    assert!(
        !names.contains(&"rbx".to_string()),
        "`rbx` is the binary this workspace ships; guarding it would flag every correct hint"
    );
}

/// The walk must reach every crate, plus the docs, not merely "some file".
///
/// One crate dropping out of the scan is invisible from the totals: the check
/// above still passes, on less. Same erosion the sibling check in
/// `env_var_names.rs` guards against.
#[test]
fn the_walk_reaches_every_crate_and_the_docs() {
    let root = workspace_root();
    let files = scanned_files(&root);

    let mut crates: Vec<String> = fs::read_dir(root.join("crates"))
        .expect("crates/ exists")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().join("Cargo.toml").is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    crates.sort();
    assert!(
        !crates.is_empty(),
        "no crates found under {}",
        root.display()
    );

    let unscanned: Vec<&String> = crates
        .iter()
        .filter(|krate| {
            let prefix = root.join("crates").join(krate);
            !files.iter().any(|path| path.starts_with(&prefix))
        })
        .collect();
    assert!(
        unscanned.is_empty(),
        "these crates contributed no file to the scan, so a dead binary name in them would not \
         be caught: {unscanned:?}.\n\
         Either the walk in collect() skips a directory it should not, or the crate layout changed."
    );

    let docs = root.join("docs");
    assert!(
        files.iter().any(|path| path.starts_with(&docs)),
        "no Markdown reached the scan from {}; the docs are where a stale command survives \
         longest, because nothing compiles them",
        docs.display()
    );
}
