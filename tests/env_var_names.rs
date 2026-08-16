//! Error messages must name environment variables the tool actually reads.
//!
//! The suite used to be a set of standalone binaries, each with its own
//! `RBX<TOOL>_API_KEY` / `RBX<TOOL>_COOKIE`. Merging them into one binary
//! unified those to `RBX_API_KEY` and `RBX_COOKIE`, but eight error messages
//! across five crates kept telling users to set the old names — variables
//! nothing reads any more. That failure is invisible from the inside: the user
//! follows the instruction, exports `RBXMETA_COOKIE`, re-runs, gets the exact
//! same error, and has nothing to go on.
//!
//! Cheaper to assert than to re-audit. The names live in string literals, so
//! no amount of type-checking would have caught it.
//!
//! # What the matcher recognises
//!
//! One shape, deliberately: an uppercase `RBX<SOMETHING>_API_KEY` or
//! `RBX<SOMETHING>_COOKIE` appearing **contiguously** in the text of a line.
//! That is how every one of these names is written today, in an error message
//! or a clap `env =` attribute.
//!
//! A name that is never contiguous in the source — `format!("RBX{tool}_COOKIE")`
//! or a `concat!` — is invisible to it, and would reintroduce the exact bug
//! this file exists for while every test stayed green.
//! [`no_environment_variable_name_is_assembled_at_runtime`] closes that door
//! rather than trusting nobody opens it; the two guards below check that the
//! walk still reaches every crate and that the matcher still matches.

use std::fs;
use std::path::{Path, PathBuf};

/// Per-tool names that are still genuinely read somewhere, with the reason.
///
/// `RBXAPIKEY_COOKIE` predates the merge and is still honoured, now as one of
/// the explicit sources in `GlobalFlags::resolve_cookie` — it used to live in
/// `rbx-apikey`'s own `resolve_cookie_from_env`, which is what let
/// `--no-auto-cookie` be ignored (#20). Kept rather than dropped because
/// removing a variable that works today would break whoever set it, and it
/// costs nothing to keep reading. Anything not on this list is a typo or a
/// leftover.
const STILL_READ: &[&str] = &["RBXAPIKEY_COOKIE"];

/// This file's own name, skipped during the walk — see the loop below.
const SELF: &str = "env_var_names.rs";

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/crates/rbx-core.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("rbx-core should live two levels below the workspace root")
        .to_path_buf()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
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
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Every `RBX<SOMETHING>_API_KEY` / `RBX<SOMETHING>_COOKIE` in `text`, where
/// `<SOMETHING>` is non-empty — so the unified `RBX_API_KEY` and `RBX_COOKIE`
/// do not match and the per-tool survivors do.
fn per_tool_names(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for suffix in ["_API_KEY", "_COOKIE"] {
        for (at, _) in text.match_indices(suffix) {
            // Walk back over the variable name to its start.
            let start = text[..at]
                .rfind(|c: char| !c.is_ascii_uppercase() && c != '_' && !c.is_ascii_digit())
                .map_or(0, |i| i + 1);
            let name = &text[start..at + suffix.len()];
            // `RBX_API_KEY` itself is the unified name and must not be flagged.
            if name.starts_with("RBX") && name.len() > 3 + suffix.len() {
                found.push(name.to_string());
            }
        }
    }
    found
}

#[test]
fn no_source_file_names_a_retired_per_tool_environment_variable() {
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(&root.join("crates"), &mut sources);
    assert!(
        !sources.is_empty(),
        "found no Rust sources under {}; the path walk is wrong, not the code",
        root.display()
    );

    let mut offenders = Vec::new();
    for path in &sources {
        // This file names the retired variables on purpose — in the module
        // docs that explain the bug, and in the matcher's own fixtures. It is
        // the one place they are allowed to appear.
        if path.file_name().is_some_and(|name| name == SELF) {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            for name in per_tool_names(line) {
                if STILL_READ.contains(&name.as_str()) {
                    continue;
                }
                let relative = path.strip_prefix(&root).unwrap_or(path);
                offenders.push(format!(
                    "{}:{}: {name}\n    {}",
                    relative.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these name per-tool environment variables that nothing reads. The unified names are \
         RBX_API_KEY and RBX_COOKIE (see GlobalFlags). If one of these really is read somewhere, \
         add it to STILL_READ in this file with the reason.\n\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_matcher_finds_a_retired_name_and_spares_the_unified_one() {
    // Guards the test itself: a matcher that silently matched nothing would
    // make the check above pass forever.
    assert_eq!(
        per_tool_names(r#"anyhow!("--api-key or RBXMETA_API_KEY is required")"#),
        vec!["RBXMETA_API_KEY"]
    );
    assert_eq!(
        per_tool_names(r#"set RBXINIT_COOKIE, or sign in"#),
        vec!["RBXINIT_COOKIE"]
    );
    assert!(per_tool_names(r#"env = "RBX_API_KEY", hide_env_values = true"#).is_empty());
    assert!(per_tool_names(r#"the RBX_COOKIE env var"#).is_empty());
}

/// The walk must reach every crate, not merely "some file somewhere".
///
/// The check above asserts only that the walk found *something*, which one
/// crate cannot move: a crate added outside `crates/`, or a directory name the
/// walk starts skipping, silently drops out while the total stays healthy.
/// This is the same erosion the API-drift extractor has (#7), in the cheaper
/// half of the pair.
#[test]
fn the_walk_reaches_every_crate_in_the_workspace() {
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(&root.join("crates"), &mut sources);

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
            !sources.iter().any(|path| path.starts_with(&prefix))
        })
        .collect();

    assert!(
        unscanned.is_empty(),
        "these crates contributed no source file to the scan, so a retired variable name in \
         them would not be caught: {unscanned:?}.\n\
         Either the walk in rust_sources() skips a directory it should not, or the crate \
         layout changed."
    );
}

/// A name assembled at runtime is a name this file cannot check.
///
/// `format!("RBX{tool}_COOKIE")` is exactly how the per-tool variables would
/// come back — one helper, every crate, and every assertion here still green
/// because no line contains the name. Refusing the shape is cheaper than
/// teaching the matcher to evaluate Rust.
#[test]
fn no_environment_variable_name_is_assembled_at_runtime() {
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(&root.join("crates"), &mut sources);

    let mut offenders = Vec::new();
    for path in &sources {
        if path.file_name().is_some_and(|name| name == SELF) {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            // `"RBX{` is the giveaway: a literal that opens the name and then
            // interpolates the middle of it.
            if line.contains("\"RBX{") || line.contains("concat!(\"RBX") {
                let relative = path.strip_prefix(&root).unwrap_or(path);
                offenders.push(format!(
                    "{}:{}\n    {}",
                    relative.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these build an environment variable name out of pieces, which makes it unverifiable \
         by the check above — the name never appears in the source, so a retired one would \
         come back unnoticed. Write the name out in full.\n\n{}",
        offenders.join("\n")
    );
}
