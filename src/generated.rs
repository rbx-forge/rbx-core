//! Shared machinery for files the CLI generates from committed inputs.
//!
//! Every generator here follows the same contract: the output is a pure
//! function of local, committed inputs (`rbxplace.toml` for `rbx env
//! gen-module`, `rbxshop.toml` + `rbxshop.lock` for `rbx shop codegen`). That
//! purity is what makes a `--check` mode meaningful: it can re-render in
//! memory and assert the committed file still matches, with no network and no
//! credentials, which is what makes it usable from a git hook or from CD.
//!
//! Two rules keep the comparison honest:
//!
//! - **One producer.** The check compares bytes, so anything else that
//!   rewrites the file (stylua, prettier) breaks it permanently, and no amount
//!   of regenerating fixes that. Keeping formatters off the generated path is
//!   the consuming project's call, not something the CLI arranges on its
//!   behalf, so [`Verdict::Formatting`] exists to name that failure instead
//!   of leaving it as a mystery diff.
//! - **One code path.** Callers build [`GeneratedFile`]s once and then either
//!   write them or check them. A verifier that re-describes what the generator
//!   emits would drift from it, and false positives are how a hook ends up
//!   disabled.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use colored::Colorize;

/// Process exit code for "a `--check` run found drift".
///
/// Distinct from the generic failure code so CI can tell "the committed file
/// is stale" (actionable: regenerate and commit) from "the command blew up"
/// (actionable: look at the error). Terraform's `-detailed-exitcode` uses the
/// same split, and keeping drift off code 1 means every other command's exit
/// status is unchanged.
pub const DRIFT_EXIT_CODE: u8 = 2;

/// Returned by a `--check` run that found at least one stale file. The binary
/// maps it to [`DRIFT_EXIT_CODE`]; nothing else about it is special.
#[derive(Debug)]
pub struct Drift {
    message: String,
}

impl Drift {
    /// Report drift found somewhere other than a byte comparison.
    ///
    /// [`CheckReport::finish`] builds one of these for the generated-file
    /// checks. `rbx check` aggregates those alongside config-against-lockfile
    /// diffs, which produce the same verdict by a different route and have to
    /// exit through the same code: otherwise "exit 2" would have two
    /// definitions in the tree and only one of them would stay correct.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Drift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Drift {}

/// One rendered file, before it is written or compared.
#[derive(Debug)]
pub struct GeneratedFile {
    pub path: PathBuf,
    pub content: String,
}

impl GeneratedFile {
    pub fn new(path: impl Into<PathBuf>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
        }
    }

    /// Write the file, creating parent directories as needed.
    pub fn write(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create {}", parent.display()))?;
            }
        }
        std::fs::write(&self.path, &self.content)
            .with_context(|| format!("Failed to write {}", self.path.display()))
    }
}

/// Where the first difference sits. `None` on a side means the file ended
/// before that line. Both `None` only terminates the scan: a difference that
/// lives entirely past the last line is whitespace, so [`Verdict::Formatting`]
/// claims it before we ever look for a line.
#[derive(Debug)]
pub struct Difference {
    pub line: usize,
    pub expected: Option<String>,
    pub actual: Option<String>,
}

#[derive(Debug)]
pub enum Verdict {
    /// On-disk content matches (ignoring CRLF vs LF).
    Match,
    /// Differs only in whitespace: a formatter that ran over the generated
    /// output, or an editor trimming trailing whitespace on save. Reported
    /// separately because the fix is "stop that tool touching this path", and
    /// regenerating would leave it failing exactly as before.
    Formatting,
    Differs(Difference),
    Missing,
    /// A file we generated in a previous run that the current inputs no longer
    /// produce (a renamed env, `typescript` turned off).
    Stale,
}

impl Verdict {
    pub fn is_drift(&self) -> bool {
        !matches!(self, Verdict::Match)
    }
}

/// Git checks out LF as CRLF on Windows under `core.autocrlf`, so a byte
/// comparison against content we rendered with `\n` would fail on every
/// Windows machine and pass in CI. Normalize both sides before comparing.
fn normalize(content: &str) -> String {
    content.replace("\r\n", "\n")
}

/// Collapse every whitespace run to a single space. Used only to label a
/// failure as formatting-related: it deliberately ignores whitespace inside
/// string literals too, which is fine for a diagnosis and never decides
/// pass/fail.
fn squeeze_whitespace(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_difference(expected: &str, actual: &str) -> Difference {
    let mut expected_lines = expected.lines();
    let mut actual_lines = actual.lines();
    let mut line = 0;
    loop {
        line += 1;
        match (expected_lines.next(), actual_lines.next()) {
            (Some(e), Some(a)) if e == a => continue,
            (e, a) => {
                return Difference {
                    line,
                    expected: e.map(str::to_string),
                    actual: a.map(str::to_string),
                }
            }
        }
    }
}

/// Compare rendered content against what is on disk.
pub fn compare(expected: &str, path: &Path) -> Result<Verdict> {
    let actual = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Verdict::Missing),
        Err(err) => return Err(err).with_context(|| format!("Failed to read {}", path.display())),
    };

    let expected = normalize(expected);
    let actual = normalize(&actual);

    if expected == actual {
        return Ok(Verdict::Match);
    }
    if squeeze_whitespace(&expected) == squeeze_whitespace(&actual) {
        return Ok(Verdict::Formatting);
    }
    Ok(Verdict::Differs(first_difference(&expected, &actual)))
}

/// Accumulates per-file verdicts and turns them into console output plus a
/// pass/fail result.
#[derive(Debug, Default)]
pub struct CheckReport {
    entries: Vec<(PathBuf, Verdict)>,
    notes: Vec<String>,
}

impl CheckReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compare one rendered file against disk and record the verdict.
    pub fn check(&mut self, file: &GeneratedFile) -> Result<()> {
        let verdict = compare(&file.content, &file.path)?;
        self.entries.push((file.path.clone(), verdict));
        Ok(())
    }

    /// Record a leftover file the current inputs no longer produce.
    pub fn stale(&mut self, path: impl Into<PathBuf>) {
        self.entries.push((path.into(), Verdict::Stale));
    }

    /// Add a caveat to the drift error, for a cause the byte comparison cannot
    /// see.
    ///
    /// [`finish`](Self::finish) otherwise names one remedy (regenerate and
    /// commit) which is right whenever the committed file is the stale side.
    /// It is wrong when the *inputs* are being read wrong, and the report has
    /// no way to notice that on its own: the render it compares against is
    /// already the misreading. Callers that know of such a cause say so here,
    /// and the note only prints when there is drift to explain.
    pub fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    pub fn has_drift(&self) -> bool {
        self.entries.iter().any(|(_, v)| v.is_drift())
    }

    /// Print every verdict and return `Err(Drift)` if any file is stale.
    ///
    /// `inputs` names what the files are derived from and `fix` is the exact
    /// command that repairs them: both end up in the error, because a check
    /// that reports drift without saying how to fix it is a check people
    /// switch off.
    pub fn finish(self, inputs: &str, fix: &str) -> Result<()> {
        let mut drifted = 0;
        let mut formatting_only = 0;

        for (path, verdict) in &self.entries {
            match verdict {
                Verdict::Match => {
                    println!("{} {}", "✓".green(), path.display());
                }
                Verdict::Missing => {
                    drifted += 1;
                    println!("{} {} {}", "✗".red(), path.display(), "(missing)".red());
                }
                Verdict::Stale => {
                    drifted += 1;
                    println!(
                        "{} {} {}",
                        "✗".red(),
                        path.display(),
                        "(not produced by the current inputs)".red()
                    );
                }
                Verdict::Formatting => {
                    drifted += 1;
                    formatting_only += 1;
                    println!(
                        "{} {} {}",
                        "✗".red(),
                        path.display(),
                        "(whitespace only)".yellow()
                    );
                }
                Verdict::Differs(diff) => {
                    drifted += 1;
                    println!(
                        "{} {} {}",
                        "✗".red(),
                        path.display(),
                        format!("(differs at line {})", diff.line).red()
                    );
                    match (&diff.expected, &diff.actual) {
                        (None, None) => {
                            println!("    {}", "trailing newline differs".dimmed());
                        }
                        (expected, actual) => {
                            println!(
                                "    {} {}",
                                "expected:".dimmed(),
                                expected.as_deref().unwrap_or("<end of file>")
                            );
                            println!(
                                "    {}   {}",
                                "actual:".dimmed(),
                                actual.as_deref().unwrap_or("<end of file>")
                            );
                        }
                    }
                }
            }
        }

        if drifted == 0 {
            println!(
                "\n{} {} generated file{} up to date.",
                "✓".green(),
                self.entries.len(),
                if self.entries.len() == 1 { "" } else { "s" }
            );
            return Ok(());
        }

        let mut message = format!(
            "{} generated file{} no longer match{} {}. Run `{}` and commit the result{}",
            drifted,
            if drifted == 1 { "" } else { "s" },
            if drifted == 1 { "es" } else { "" },
            inputs,
            fix,
            // The advice is right when the committed file is the stale side,
            // which is the usual case but not the only one, so it stops being
            // stated as the single remedy the moment a caller knows otherwise.
            if self.notes.is_empty() {
                "."
            } else {
                ", unless one of the following applies."
            },
        );

        if formatting_only > 0 {
            let scope = if formatting_only == drifted {
                "Every difference above is"
            } else {
                "Some of the differences above are"
            };
            message.push_str(&format!(
                "\n\n{scope} whitespace only, so something else is rewriting these files: \
                 a formatter (stylua, prettier), or an editor trimming whitespace on save. \
                 Regenerating will not make that stop: exclude the generated path from \
                 whatever is touching it (a .styluaignore / .prettierignore entry)."
            ));
        }

        for note in &self.notes {
            message.push_str("\n\n");
            message.push_str(note);
        }

        Err(Drift { message }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn identical_content_matches() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "a.luau", "return 1\n");
        assert!(matches!(
            compare("return 1\n", &path).unwrap(),
            Verdict::Match
        ));
    }

    #[test]
    fn crlf_on_disk_still_matches_lf_output() {
        // The Windows case: git hands back CRLF, we render LF. Without
        // normalization every check would fail on a dev machine and pass in CI.
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "a.luau", "local x = 1\r\nreturn x\r\n");
        assert!(matches!(
            compare("local x = 1\nreturn x\n", &path).unwrap(),
            Verdict::Match
        ));
    }

    #[test]
    fn reindented_content_is_reported_as_formatting() {
        let dir = tempdir().unwrap();
        // What stylua does: same tokens, different indentation.
        let path = write(dir.path(), "a.luau", "return {\n    x = 1,\n}\n");
        assert!(matches!(
            compare("return {\n\tx = 1,\n}\n", &path).unwrap(),
            Verdict::Formatting
        ));
    }

    #[test]
    fn changed_value_is_a_real_difference_with_its_line() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "a.luau", "return {\n\tx = 2,\n}\n");
        let verdict = compare("return {\n\tx = 1,\n}\n", &path).unwrap();
        let Verdict::Differs(diff) = verdict else {
            panic!("expected a real difference, got {verdict:?}");
        };
        assert_eq!(diff.line, 2);
        assert_eq!(diff.expected.as_deref(), Some("\tx = 1,"));
        assert_eq!(diff.actual.as_deref(), Some("\tx = 2,"));
    }

    #[test]
    fn a_stripped_trailing_newline_counts_as_formatting() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "a.luau", "return 1");
        // Still drift, but an editor's "trim on save", not a stale id, and
        // the report has to say so or the advice ("regenerate") is wrong.
        assert!(matches!(
            compare("return 1\n", &path).unwrap(),
            Verdict::Formatting
        ));
    }

    #[test]
    fn an_absent_file_is_missing_not_an_error() {
        let dir = tempdir().unwrap();
        let verdict = compare("return 1\n", &dir.path().join("nope.luau")).unwrap();
        assert!(matches!(verdict, Verdict::Missing));
    }

    #[test]
    fn a_clean_report_passes_and_a_dirty_one_names_the_fix() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "a.luau", "return 1\n");

        let mut clean = CheckReport::new();
        clean
            .check(&GeneratedFile::new(&path, "return 1\n"))
            .unwrap();
        assert!(!clean.has_drift());
        clean.finish("inputs", "rbx shop codegen").unwrap();

        let mut dirty = CheckReport::new();
        dirty
            .check(&GeneratedFile::new(&path, "return 2\n"))
            .unwrap();
        assert!(dirty.has_drift());
        let err = dirty
            .finish("rbxshop.lock", "rbx shop codegen")
            .unwrap_err();
        assert!(err.chain().any(|c| c.is::<Drift>()));
        assert!(err.to_string().contains("rbx shop codegen"));
    }

    #[test]
    fn a_note_qualifies_the_regenerate_advice_instead_of_being_appended_to_it() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "a.luau", "return 1\n");
        let mut report = CheckReport::new();
        report
            .check(&GeneratedFile::new(&path, "return 2\n"))
            .unwrap();
        report.note("the inputs may be read wrong");
        let err = report
            .finish("rbxplace.toml", "rbx env gen-module")
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("the inputs may be read wrong"),
            "got: {message}"
        );
        // The fix must stop being stated as the only remedy, not merely have a
        // caveat stapled after it.
        assert!(
            message.contains("unless one of the following applies"),
            "got: {message}"
        );
        assert!(
            !message.contains("commit the result."),
            "the unconditional full stop must be gone: {message}"
        );
    }

    #[test]
    fn a_clean_report_never_prints_its_notes() {
        // Notes explain drift. With nothing to explain they are noise, and a
        // green check that lectures gets its output skimmed.
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "a.luau", "return 1\n");
        let mut report = CheckReport::new();
        report
            .check(&GeneratedFile::new(&path, "return 1\n"))
            .unwrap();
        report.note("never printed");
        report
            .finish("rbxplace.toml", "rbx env gen-module")
            .unwrap();
    }

    #[test]
    fn a_formatting_only_report_says_regenerating_wont_help() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "a.luau", "return {\n    x = 1,\n}\n");
        let mut report = CheckReport::new();
        report
            .check(&GeneratedFile::new(&path, "return {\n\tx = 1,\n}\n"))
            .unwrap();
        let err = report
            .finish("rbxshop.lock", "rbx shop codegen")
            .unwrap_err();
        assert!(err.to_string().contains("formatter"));
    }
}
