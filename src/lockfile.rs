//! Shared lockfile loading: version validation and stepwise migration.
//!
//! Every lockfile in the suite (`rbxmeta.lock.toml`, `rbxshop.lock.toml`, ...)
//! carries a `version` integer. Writing it is easy; the value only earns its
//! keep if something reads it back. Three behaviours live here so each lockfile
//! inherits all of them at once:
//!
//! 1. **Refuse anything newer than this build.** A lockfile written by a newer
//!    `rbx` is not something we can safely guess at, and parsing it as the
//!    current version would drop or misread state without saying so. The error
//!    names both versions and points at the real fix (upgrade) rather than
//!    surfacing a parse failure the user cannot act on.
//! 2. **Migrate older versions forward, one step at a time**, through
//!    [`LockfileFormat::migrations`]. Registries are keyed `"N -> N+1"` and
//!    applied in sequence until the file reaches the current version.
//! 3. **One helper**, so behaviour cannot drift between lockfiles.
//!
//! Writing goes through [`write()`] for the same reason: a `sync` rewrites its
//! lockfile after every resource it creates, and the retry that keeps an
//! editor from stopping that run halfway belongs to every lockfile at once.
//!
//! The migration registries are empty today, and that is the point: the
//! enforcement point has to exist *before* lockfiles are in the wild, because
//! it is the part that cannot be retrofitted cheaply afterwards. This mirrors
//! the project's own "an ignored key must not pass for applied" policy for
//! `rbx env` (see `docs/env.md`): a version we silently ignore is a version we
//! silently claim to have honoured.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;

/// How many times [`write()`] tries before giving up.
const WRITE_ATTEMPTS: u32 = 5;

/// Wait before retry `n` (1-based) grows linearly: 50, 100, 150, 200 ms, so
/// the worst case adds half a second to a run that would otherwise have died.
const WRITE_BACKOFF: Duration = Duration::from_millis(50);

/// Write a lockfile, riding out another process that holds it for a moment.
///
/// A `sync` rewrites its lockfile once per resource, right after the remote
/// call that created it. On Windows, anything that reacts to that change (an
/// editor's TOML extension, the search indexer, Defender) may have the file
/// mapped or open when the next write lands, and the write then fails with
/// `os error 1224`. Nothing is wrong with the file: the reader lets go within
/// milliseconds. Failing there anyway stops the run with a resource created on
/// Roblox and missing from the lockfile, so a short retry is the cheaper path.
///
/// Only those transient codes are retried. A full disk or a missing directory
/// fails on the first attempt, as before.
///
/// The file is replaced in one step and never written in place: the content
/// is staged beside it and renamed over it, so an interrupted write leaves the
/// lockfile as it was, never empty. A staged copy left by a run that was
/// killed is removed by the next write.
///
/// This is for a lockfile of ids, which a repository carries. A file that
/// holds a credential goes through [`write_private`], which stages nothing.
pub fn write(path: &Path, content: &str) -> Result<()> {
    write_with(path, content, replace, std::thread::sleep)
}

/// Write a file that holds a credential, readable by its owner only.
///
/// Same retry as [`write()`], and on purpose not the same replacement. A
/// staged copy would be a second file holding the secret, under a name that
/// no ignore rule written for the first one covers: a run killed before the
/// rename would leave it one `git add` away from being published. So this
/// file is written in place, and what protects it is the order of the steps:
///
/// - It is never emptied first. The content is written over the old one, and
///   the file is cut to length afterwards.
/// - If a step fails, the old content is put back. It fits where it was, so a
///   full disk leaves the file as it was. If putting it back fails too, the
///   error says the file may now be inconsistent.
/// - A file this call created is removed if any step fails, the restriction
///   below included. An empty lockfile is refused for having no version; an
///   absent one is the ordinary start.
///
/// What this does not give is the all-or-nothing of a rename: a machine that
/// loses power during the write can leave the two contents mixed.
///
/// Who can read the result:
///
/// - **Unix:** the file is created with mode `0600`, so there is no moment where
///   it exists with the umask's default (usually `0644`). `mode` only applies at
///   creation, so a file that already existed is tightened before the secret is
///   written into it.
/// - **Windows:** inheritance is removed and the current user is the only
///   account granted access, through `icacls`, before the secret is written. A
///   file in a profile folder is already private by inheritance, but one on
///   another drive (a repository on `D:\`, say) usually inherits a grant to
///   every authenticated user. If `icacls` fails, a warning names the file and
///   the write still succeeds: failing would lose a secret Roblox shows only
///   once. It runs on the first write of a file in a process, and on any write
///   that creates the file, not on every write: a file this process restricted
///   and then wrote again keeps the access it was given. A file another
///   program replaces under the same name while the process runs is the one
///   case that is not restricted again.
pub fn write_private(path: &Path, content: &str) -> Result<()> {
    write_with(path, content, overwrite_owner_only, std::thread::sleep)
}

/// Write `content` over `path` in place, with the file restricted to its
/// owner before the first byte goes in.
fn overwrite_owner_only(path: &Path, content: &str) -> io::Result<()> {
    overwrite_with(path, content, restrict_to_owner, put)
}

/// [`overwrite_owner_only`], with the restriction and the write injected so a
/// test can fail either and see what is left.
fn overwrite_with(
    path: &Path,
    content: &str,
    restrict: impl FnOnce(&std::fs::File, &Path, bool) -> io::Result<()>,
    write: impl FnOnce(&mut std::fs::File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let (mut file, created) = open_or_create(path)?;
    let filled = fill(&mut file, path, created, content, restrict, write);
    if filled.is_err() && created {
        // There was no file, and an empty one is not the same thing: a
        // lockfile that parses to nothing is refused, an absent one is not.
        // So whichever step failed, the file this call created goes. Best
        // effort: the error worth reporting is the one that stopped the write.
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    filled
}

/// Restrict `file` to its owner, then write `content` over what it holds, and
/// put that back if the write does not go through.
fn fill(
    file: &mut std::fs::File,
    path: &Path,
    created: bool,
    content: &str,
    restrict: impl FnOnce(&std::fs::File, &Path, bool) -> io::Result<()>,
    write: impl FnOnce(&mut std::fs::File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Read;

    restrict(file, path, created)?;
    // What the file holds now, to put back if the write does not go through.
    let mut before = Vec::new();
    file.read_to_end(&mut before)?;

    let Err(err) = write(file, content.as_bytes()) else {
        return Ok(());
    };
    if created {
        // Nothing to put back: the caller removes the file.
        return Err(err);
    }
    match put(file, &before) {
        Ok(()) => Err(err),
        Err(restore) => Err(io::Error::new(
            err.kind(),
            NotRestored {
                write: err,
                restore,
            },
        )),
    }
}

/// A write that failed, and the undo that failed after it.
///
/// The file then holds neither the old content nor the new one for certain,
/// which whoever reads the error has to be told. The write's error stays the
/// cause, since it is the one that says what went wrong first.
#[derive(Debug)]
struct NotRestored {
    write: io::Error,
    restore: io::Error,
}

impl std::fmt::Display for NotRestored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the write failed and the old content could not be put back ({}), so the file \
             may now hold part of each and be inconsistent",
            self.restore
        )
    }
}

impl std::error::Error for NotRestored {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.write)
    }
}

/// Make `file` hold exactly `bytes`.
///
/// Cut after the write and never before, which is the whole point: a file is
/// not emptied in order to be refilled.
fn put(file: &mut std::fs::File, bytes: &[u8]) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.set_len(bytes.len() as u64)?;
    file.sync_all()
}

/// Open `path` for an in-place write, creating it if it is not there. Says
/// whether it was created.
///
/// On Unix a file this creates has mode `0600` from the start, so there is no
/// moment where it exists with the umask's default.
fn open_or_create(path: &Path) -> io::Result<(std::fs::File, bool)> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    match options.clone().create_new(true).open(path) {
        Ok(file) => Ok((file, true)),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok((options.open(path)?, false)),
        Err(err) => Err(err),
    }
}

/// Leave `file` readable by its owner alone. `created` says whether this
/// write just created it.
#[cfg(unix)]
fn restrict_to_owner(file: &std::fs::File, _path: &Path, _created: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // `mode` is filtered by the umask at creation and ignored on a file that
    // was already there: set it outright.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

/// Leave `file` readable by its owner alone. `created` says whether this
/// write just created it.
///
/// `icacls` runs once per file per process, not on every write: a tool that
/// saves after each key, or a write that is retried, finds the file already
/// restricted. A file the write just created is always restricted, since it
/// is a new file with the access its directory gives.
#[cfg(windows)]
fn restrict_to_owner(_file: &std::fs::File, path: &Path, created: bool) -> io::Result<()> {
    static RESTRICTED: Restricted = Restricted::new();
    RESTRICTED.restrict(path, created, restrict_to_current_user);
    Ok(())
}

/// The files this process has restricted to its user.
#[cfg(any(windows, test))]
#[derive(Debug)]
struct Restricted(std::sync::Mutex<std::collections::BTreeSet<PathBuf>>);

#[cfg(any(windows, test))]
impl Restricted {
    const fn new() -> Self {
        Self(std::sync::Mutex::new(std::collections::BTreeSet::new()))
    }

    /// Run `restrict` on `path`, unless this process already did and the file
    /// is not one the write just created. `restrict` says whether it worked:
    /// one that did not is tried again on the next write.
    fn restrict(&self, path: &Path, created: bool, restrict: impl FnOnce(&Path) -> bool) {
        let known = || {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        if !created && known().contains(path) {
            return;
        }
        if restrict(path) {
            known().insert(path.to_path_buf());
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn restrict_to_owner(_file: &std::fs::File, _path: &Path, _created: bool) -> io::Result<()> {
    Ok(())
}

/// Replace `path` with `content` in one step.
///
/// The content is written to a sibling file, flushed to disk, and renamed over
/// `path`. A rename within one directory is atomic, so a run that is killed
/// partway, or a disk that fills, leaves the file exactly as it was. Writing
/// in place truncated it first: the same interruption left it empty, and to a
/// lockfile empty means every resource it tracked was never created.
///
/// On Unix the new file takes the mode of the one it replaces. On Windows it
/// takes the access its directory gives a new file.
fn replace(path: &Path, content: &str) -> io::Result<()> {
    replace_with(path, content, |from, to| std::fs::rename(from, to))
}

/// [`replace`], with the rename injected so a test can interrupt the write at
/// the one point where it matters.
fn replace_with(
    path: &Path,
    content: &str,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    // Through a symlink, the file to replace is the one it points at: renaming
    // over the link would swap the link for a regular file.
    let target = if path.is_symlink() {
        std::fs::canonicalize(path)?
    } else {
        path.to_path_buf()
    };
    let sibling = sibling_of(&target);
    remove_stale_siblings(&target, &sibling);

    let written =
        write_sibling(&sibling, &target, content).and_then(|()| rename(&sibling, &target));
    match written {
        Ok(()) => {
            sync_parent(&target);
            Ok(())
        }
        Err(err) => {
            // Best effort: the error worth reporting is the one that stopped
            // the write, not a failure to tidy up after it.
            let _ = std::fs::remove_file(&sibling);
            Err(err)
        }
    }
}

/// The file the content is staged in: beside `target`, so the rename never
/// crosses a volume, and named after this process, so two runs in one
/// directory do not stage into each other's file.
fn sibling_of(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

/// How old a staged copy has to be before it is taken for a dead run's.
///
/// A live run stages and renames within milliseconds, so a minute is far past
/// anything still being written by another process.
const STALE_AFTER: Duration = Duration::from_secs(60);

/// Remove the staged copies that killed runs left beside `target`.
///
/// Best effort, and never this run's own: a leftover is clutter in a
/// directory somebody commits from, not a reason to fail the write.
fn remove_stale_siblings(target: &Path, own: &Path) {
    let Some(name) = target.file_name() else {
        return;
    };
    let prefix = format!(".{}.", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(parent_of(target)) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        if Some(file_name.as_os_str()) == own.file_name() {
            continue;
        }
        // `.<name>.<pid>.tmp`, and nothing that merely looks like it.
        let staged_by_a_run = file_name
            .to_string_lossy()
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".tmp"))
            .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()));
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age >= STALE_AFTER);
        if staged_by_a_run && stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The directory `target` is in, which for a bare file name is the current
/// one.
fn parent_of(target: &Path) -> &Path {
    match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn write_sibling(sibling: &Path, target: &Path, content: &str) -> io::Result<()> {
    use std::io::Write;

    // One left behind by a run that died is stale by definition.
    let _ = std::fs::remove_file(sibling);
    let mut file = create_sibling(sibling, target)?;
    file.write_all(content.as_bytes())?;
    // On disk before the rename. A rename that lands ahead of the data it
    // names is how a crash swaps a whole file for an empty one.
    file.sync_all()
}

#[cfg(unix)]
fn create_sibling(sibling: &Path, target: &Path) -> io::Result<std::fs::File> {
    use std::fs::{OpenOptions, Permissions};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    // The mode the file had, so one somebody tightened stays tightened. A new
    // file takes the umask's default.
    let mode = std::fs::metadata(target)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o777);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    if let Some(mode) = mode {
        options.mode(mode);
    }
    let file = options.open(sibling)?;
    if let Some(mode) = mode {
        // `mode` is filtered by the umask at creation: set it outright.
        file.set_permissions(Permissions::from_mode(mode))?;
    }
    Ok(file)
}

#[cfg(not(unix))]
fn create_sibling(sibling: &Path, _target: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(sibling)
}

/// Flush the directory entry the rename wrote, best effort: without it the
/// rename itself can be lost to a power cut on some filesystems.
#[cfg(unix)]
fn sync_parent(target: &Path) {
    if let Ok(dir) = std::fs::File::open(parent_of(target)) {
        let _ = dir.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent(_target: &Path) {}

/// Drop inherited access and grant the current user alone, best effort. Says
/// whether it worked, after a warning when it did not.
#[cfg(windows)]
fn restrict_to_current_user(path: &Path) -> bool {
    let user = std::env::var("USERNAME").unwrap_or_default();
    let granted = !user.is_empty()
        && std::process::Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{user}:(F)"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    if !granted {
        eprintln!(
            "warning: could not restrict {} to your account; check who can read it",
            path.display()
        );
    }
    granted
}

fn write_with(
    path: &Path,
    content: &str,
    mut attempt: impl FnMut(&Path, &str) -> io::Result<()>,
    mut sleep: impl FnMut(Duration),
) -> Result<()> {
    let mut tries = 1;
    loop {
        match attempt(path, content) {
            Ok(()) => return Ok(()),
            Err(err) if tries < WRITE_ATTEMPTS && is_transient(&err) => {
                sleep(WRITE_BACKOFF * tries);
                tries += 1;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("Failed to write {}", path.display()))
            }
        }
    }
}

/// The Windows codes a reader holding the file produces, and nothing else.
///
/// 1224 is a mapped section (`ERROR_USER_MAPPED_FILE`), 32 a sharing violation,
/// 5 the access denial a pending delete or a scanner gives. On other platforms
/// a write does not fail because someone is reading, so nothing is retried.
fn is_transient(err: &io::Error) -> bool {
    cfg!(windows) && matches!(err.raw_os_error(), Some(1224 | 32 | 5))
}

/// One step of a lockfile format migration.
///
/// `apply` receives the whole document as a mutable [`toml::Value`], before it
/// is deserialized into the concrete lockfile type, so a step is free to
/// rename, reshape, or drop keys that the current struct no longer knows about.
#[derive(Clone, Copy)]
pub struct LockfileMigration {
    /// The step, written the way the registry reads: `"1 -> 2"`.
    ///
    /// This is the identity of the step, not decoration: [`LockfileFormat`]
    /// parses the left-hand version out of it to decide when to run.
    pub step: &'static str,
    pub apply: fn(&mut toml::Value) -> Result<()>,
}

impl std::fmt::Debug for LockfileMigration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockfileMigration")
            .field("step", &self.step)
            .finish_non_exhaustive()
    }
}

impl LockfileMigration {
    /// The version this step migrates *from*, parsed out of `step`.
    fn source_version(&self) -> Result<u32> {
        let (from, to) = self.step.split_once("->").ok_or_else(|| {
            anyhow!(
                "malformed migration key {:?}: expected \"N -> N+1\"",
                self.step
            )
        })?;
        let from: u32 = from
            .trim()
            .parse()
            .with_context(|| format!("malformed migration key {:?}", self.step))?;
        let to: u32 = to
            .trim()
            .parse()
            .with_context(|| format!("malformed migration key {:?}", self.step))?;
        if to != from + 1 {
            bail!(
                "migration key {:?} is not a single step: {} -> {}",
                self.step,
                from,
                to
            );
        }
        Ok(from)
    }
}

/// The version contract for one lockfile format.
#[derive(Debug, Clone, Copy)]
pub struct LockfileFormat {
    /// File name, used verbatim in error messages (e.g. `rbxmeta.lock.toml`).
    pub name: &'static str,
    /// The version this build writes and understands.
    pub current: u32,
    /// Stepwise migrations, keyed `"N -> N+1"`. Order does not matter; steps
    /// are looked up by the version they migrate from.
    pub migrations: &'static [LockfileMigration],
}

impl LockfileFormat {
    /// Read, validate and migrate the lockfile at `path`.
    ///
    /// Returns `Ok(None)` when the file does not exist: a first run is not an
    /// error, and each caller has its own idea of what an empty lockfile looks
    /// like.
    ///
    /// On success the returned value always carries `version = current`: any
    /// pending migrations have been applied, so the caller never has to think
    /// about the on-disk version again.
    pub fn load<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let mut doc: toml::Value = content
            .parse()
            .with_context(|| format!("Failed to parse {}", path.display()))?;

        let found = self.read_version(&doc, path)?;
        self.migrate(&mut doc, found, path)?;

        if let Some(table) = doc.as_table_mut() {
            table.insert(
                "version".to_string(),
                toml::Value::Integer(self.current.into()),
            );
        }

        let parsed = doc
            .try_into()
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok(Some(parsed))
    }

    fn read_version(&self, doc: &toml::Value, path: &Path) -> Result<u32> {
        let raw = doc.get("version").ok_or_else(|| {
            anyhow!(
                "{} has no `version` key.\n  \
                 Every {} carries the lockfile format version it was written with.\n  \
                 Fix: delete the file and re-run to regenerate it, or add `version = {}` if you know it is current.",
                path.display(),
                self.name,
                self.current
            )
        })?;
        let n = raw.as_integer().ok_or_else(|| {
            anyhow!(
                "{}: `version` must be an integer, found {}.",
                path.display(),
                raw.type_str()
            )
        })?;
        u32::try_from(n).map_err(|_| {
            anyhow!(
                "{}: `version` must be a non-negative integer that fits in 32 bits, found {}.",
                path.display(),
                n
            )
        })
    }

    /// Walk `found` up to `self.current`, applying one registered step per
    /// version. Refuses anything newer than this build understands.
    fn migrate(&self, doc: &mut toml::Value, found: u32, path: &Path) -> Result<()> {
        if found > self.current {
            bail!(
                "{} was written by a newer rbx (lockfile version {}); this build understands version {}.\n  \
                 Fix: update rbx.\n  \
                 Refusing to continue: reading a version {} file as version {} would silently drop or misread state.",
                path.display(),
                found,
                self.current,
                found,
                self.current
            );
        }

        let mut version = found;
        while version < self.current {
            let step = self.step_from(version)?.ok_or_else(|| {
                anyhow!(
                    "{} is at lockfile version {}, and this build has no migration registered for \"{} -> {}\" (current version {}).\n  \
                     Fix: delete the file and re-run to regenerate it.",
                    path.display(),
                    version,
                    version,
                    version + 1,
                    self.current
                )
            })?;
            (step.apply)(doc)
                .with_context(|| format!("Failed to migrate {} ({})", path.display(), step.step))?;
            version += 1;
        }
        Ok(())
    }

    fn step_from(&self, version: u32) -> Result<Option<&LockfileMigration>> {
        for m in self.migrations {
            if m.source_version()? == version {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }

    /// Every registered step is well-formed, single-step, and unique.
    ///
    /// Meant to be called from a crate's own test so a malformed registry fails
    /// that crate's suite rather than a user's `rbx` run. It is cheap and has no
    /// side effects, so calling it in production code is fine too.
    pub fn validate_registry(&self) -> Result<()> {
        let mut seen: Vec<u32> = Vec::with_capacity(self.migrations.len());
        for m in self.migrations {
            let from = m.source_version()?;
            if from >= self.current {
                bail!(
                    "migration {:?} starts at or past the current version {}",
                    m.step,
                    self.current
                );
            }
            if seen.contains(&from) {
                bail!("two migrations registered for version {}", from);
            }
            seen.push(from);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Deserialize)]
    struct Fake {
        version: u32,
        #[serde(default)]
        value: String,
    }

    const V1_TO_V2: LockfileMigration = LockfileMigration {
        step: "1 -> 2",
        apply: |doc| {
            let table = doc.as_table_mut().expect("document root is a table");
            // v1 spelled it `old_value`; v2 calls it `value`.
            if let Some(old) = table.remove("old_value") {
                table.insert("value".to_string(), old);
            }
            Ok(())
        },
    };

    fn format(current: u32, migrations: &'static [LockfileMigration]) -> LockfileFormat {
        LockfileFormat {
            name: "fake.lock.toml",
            current,
            migrations,
        }
    }

    fn write(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fake.lock.toml");
        std::fs::write(&path, content).expect("write");
        (dir, path)
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loaded: Option<Fake> = format(2, &[])
            .load(&dir.path().join("absent.lock.toml"))
            .expect("missing file");
        assert!(loaded.is_none());
    }

    #[test]
    fn a_current_version_file_loads_unchanged() {
        let (_d, path) = write("version = 2\nvalue = \"kept\"\n");
        let loaded: Fake = format(2, &[]).load(&path).expect("load").expect("present");
        assert_eq!(
            loaded,
            Fake {
                version: 2,
                value: "kept".to_string()
            }
        );
    }

    #[test]
    fn a_newer_version_is_refused_and_the_error_names_both_versions() {
        let (_d, path) = write("version = 7\n");
        let err = format(2, &[])
            .load::<Fake>(&path)
            .expect_err("a newer lockfile must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("version 7"), "{msg}");
        assert!(msg.contains("version 2"), "{msg}");
        assert!(
            msg.contains("update rbx"),
            "the error must point at the upgrade, not at a parse failure: {msg}"
        );
    }

    #[test]
    fn an_older_version_is_migrated_step_by_step() {
        static STEPS: &[LockfileMigration] = &[V1_TO_V2];
        let (_d, path) = write("version = 1\nold_value = \"carried\"\n");
        let loaded: Fake = format(2, STEPS)
            .load(&path)
            .expect("load")
            .expect("present");
        assert_eq!(
            loaded,
            Fake {
                version: 2,
                value: "carried".to_string()
            },
            "the migrated document must come back stamped with the current version"
        );
    }

    #[test]
    fn an_older_version_with_no_registered_migration_is_refused() {
        let (_d, path) = write("version = 1\n");
        let err = format(2, &[])
            .load::<Fake>(&path)
            .expect_err("an unmigratable lockfile must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("\"1 -> 2\""), "{msg}");
    }

    #[test]
    fn a_missing_version_key_is_refused_with_an_actionable_message() {
        let (_d, path) = write("value = \"orphan\"\n");
        let err = format(2, &[])
            .load::<Fake>(&path)
            .expect_err("a lockfile without a version must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("no `version` key"), "{msg}");
    }

    #[test]
    fn a_non_integer_version_is_refused() {
        let (_d, path) = write("version = \"two\"\n");
        let err = format(2, &[])
            .load::<Fake>(&path)
            .expect_err("a non-integer version must be refused");
        assert!(format!("{err:#}").contains("must be an integer"), "{err:#}");
    }

    #[test]
    fn migration_keys_are_parsed_out_of_their_n_to_n_plus_1_spelling() {
        assert_eq!(V1_TO_V2.source_version().expect("parses"), 1);
        assert_eq!(
            LockfileMigration {
                step: "10->11",
                apply: |_| Ok(())
            }
            .source_version()
            .expect("parses without spaces"),
            10
        );
    }

    #[test]
    fn a_malformed_migration_key_is_rejected_rather_than_skipped() {
        for step in ["1 to 2", "1 -> 3", "x -> y", "2"] {
            let m = LockfileMigration {
                step,
                apply: |_| Ok(()),
            };
            assert!(
                m.source_version().is_err(),
                "{step:?} should not parse as a single migration step"
            );
        }
    }

    /// Drive `write_with` against a writer that fails with `codes` in order,
    /// then succeeds. Returns the result, the attempts made and the waits.
    fn write_failing_with(codes: &[i32]) -> (Result<()>, usize, Vec<Duration>) {
        let mut failures = codes.iter();
        let mut attempts = 0;
        let mut waits = Vec::new();
        let result = write_with(
            Path::new("fake.lock.toml"),
            "version = 1\n",
            |_, _| {
                attempts += 1;
                match failures.next() {
                    Some(&code) => Err(io::Error::from_raw_os_error(code)),
                    None => Ok(()),
                }
            },
            |d| waits.push(d),
        );
        (result, attempts, waits)
    }

    #[test]
    fn write_puts_the_content_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fake.lock.toml");
        super::write(&path, "version = 1\n").expect("write");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "version = 1\n"
        );
    }

    #[test]
    fn write_private_puts_the_content_on_disk_and_replaces_an_older_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.lock.toml");
        write_private(&path, "first, and a longer line\n").expect("write");
        write_private(&path, "second\n").expect("rewrite");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "second\n");
    }

    #[cfg(unix)]
    #[test]
    fn write_private_creates_the_file_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.lock.toml");
        write_private(&path, "secret\n").expect("write");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {mode:o}");
    }

    /// A file that already existed with the default mode is tightened too:
    /// `mode()` alone would leave it alone, because it only applies at creation.
    #[cfg(unix)]
    #[test]
    fn write_private_tightens_a_file_that_was_already_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.lock.toml");
        std::fs::write(&path, "old\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_private(&path, "secret\n").expect("write");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {mode:o}");
    }

    /// `icacls` marks every inherited entry `(I)`. After the write there must be
    /// none, and the current user must still be able to read the file.
    #[cfg(windows)]
    #[test]
    fn write_private_removes_inherited_access_on_windows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.lock.toml");
        write_private(&path, "secret\n").expect("write");
        let out = std::process::Command::new("icacls")
            .arg(&path)
            .output()
            .expect("icacls runs");
        let listing = String::from_utf8_lossy(&out.stdout);
        assert!(
            !listing.contains("(I)"),
            "inherited entries remain:\n{listing}"
        );
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "secret\n");
    }

    /// A file somebody else wrote, with the access its directory gives, is
    /// restricted on its first private write even though it was not created
    /// by it.
    #[cfg(windows)]
    #[test]
    fn write_private_restricts_a_file_that_was_already_there_on_windows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.lock.toml");
        std::fs::write(&path, "old\n").expect("seed");

        write_private(&path, "secret\n").expect("write");
        write_private(&path, "secret, again\n").expect("rewrite");

        let out = std::process::Command::new("icacls")
            .arg(&path)
            .output()
            .expect("icacls runs");
        let listing = String::from_utf8_lossy(&out.stdout);
        assert!(
            !listing.contains("(I)"),
            "inherited entries remain:\n{listing}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "secret, again\n"
        );
    }

    /// One restriction per file per process, except for a file the write just
    /// created, and except after one that did not work.
    #[test]
    fn a_file_is_restricted_once_unless_it_is_new_or_the_last_try_failed() {
        let restricted = Restricted::new();
        let runs = std::cell::RefCell::new(Vec::new());
        let write = |name: &str, created: bool, works: bool| {
            restricted.restrict(Path::new(name), created, |path| {
                runs.borrow_mut().push(path.display().to_string());
                works
            })
        };

        write("a", true, true); // created: restricted
        write("a", false, true); // written again: already restricted
        write("a", false, true); // a retry: the same
        write("b", false, true); // there before, never seen: restricted
        write("a", true, true); // created again: a new file
        write("c", true, false); // icacls failed
        write("c", false, true); // so the next write tries again

        assert_eq!(*runs.borrow(), ["a", "b", "a", "c", "c"]);
    }

    /// The names in `dir`, sorted: what a write left behind.
    fn left_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read the directory")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// The reason for the sibling file. Written in place, an interruption
    /// after the truncate left the lockfile empty, and an empty lockfile means
    /// every key or id it tracked reads as never created.
    #[test]
    fn a_write_interrupted_before_it_lands_leaves_the_file_as_it_was() {
        let (dir, path) = write("version = 1\nvalue = \"kept\"\n");

        let err = replace_with(&path, "version = 1\n", |_, _| {
            Err(io::Error::other("interrupted"))
        })
        .expect_err("the write did not land");

        assert_eq!(err.to_string(), "interrupted");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "version = 1\nvalue = \"kept\"\n"
        );
        assert_eq!(
            left_in(dir.path()),
            ["fake.lock.toml"],
            "the staged copy is removed"
        );
    }

    #[test]
    fn a_write_leaves_nothing_beside_the_file() {
        let (dir, path) = write("version = 1\n");

        super::write(&path, "version = 2\n").expect("write");
        write_private(&path, "version = 3\n").expect("write");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "version = 3\n"
        );
        assert_eq!(left_in(dir.path()), ["fake.lock.toml"]);
    }

    /// A run that died between staging and renaming leaves its sibling behind,
    /// and the next run of the same process id must not trip on it.
    #[test]
    fn a_staged_copy_left_by_a_dead_run_does_not_stop_the_next_write() {
        let (dir, path) = write("version = 1\n");
        std::fs::write(sibling_of(&path), "half a file").expect("seed the stale sibling");

        super::write(&path, "version = 2\n").expect("write");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "version = 2\n"
        );
        assert_eq!(left_in(dir.path()), ["fake.lock.toml"]);
    }

    /// A run killed between staging and renaming leaves its copy behind under
    /// its own process id, which no later run would ever reuse on purpose.
    /// The next write clears it, and leaves alone what is not provably one.
    #[test]
    fn a_staged_copy_left_by_another_dead_run_is_removed_by_the_next_write() {
        let (dir, path) = write("version = 1\n");
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(600);
        let seed = |name: &str, aged: bool| {
            let file = std::fs::File::create(dir.path().join(name)).expect("seed");
            if aged {
                file.set_modified(long_ago).expect("age the file");
            }
        };
        // Old enough, and named the way a run names its staged copy.
        seed(".fake.lock.toml.999999991.tmp", true);
        // Staged a moment ago: another run may be about to rename it.
        seed(".fake.lock.toml.999999992.tmp", false);
        // Old, but not the shape of a staged copy, or of another lockfile's.
        seed(".fake.lock.toml.notes.tmp", true);
        seed(".other.lock.toml.999999993.tmp", true);

        super::write(&path, "version = 2\n").expect("write");

        assert_eq!(
            left_in(dir.path()),
            [
                ".fake.lock.toml.999999992.tmp",
                ".fake.lock.toml.notes.tmp",
                ".other.lock.toml.999999993.tmp",
                "fake.lock.toml",
            ]
        );
    }

    /// The reason a credential is not staged: the copy would be a second file
    /// holding the secret, under a name no ignore rule for the first covers.
    #[test]
    fn a_private_write_never_puts_the_secret_in_a_second_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.lock.toml");
        write_private(&path, "old secret\n").expect("seed");

        let mut seen_during_the_write = Vec::new();
        overwrite_with(&path, "new secret\n", restrict_to_owner, |file, bytes| {
            let done = put(file, bytes);
            seen_during_the_write = left_in(dir.path());
            done
        })
        .expect("write");

        assert_eq!(seen_during_the_write, ["secret.lock.toml"]);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "new secret\n"
        );
    }

    /// A disk that fills partway through. The file was never emptied, and what
    /// the failed write put over the old content is undone.
    #[test]
    fn a_private_write_that_fails_partway_puts_the_old_content_back() {
        use std::io::{Seek, SeekFrom, Write};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.lock.toml");
        write_private(&path, "the old secret, intact\n").expect("seed");

        let err = overwrite_with(
            &path,
            "a new secret that is longer than the old one\n",
            restrict_to_owner,
            |file, bytes| {
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&bytes[..bytes.len() / 2])?;
                Err(io::Error::other("no space left"))
            },
        )
        .expect_err("the write did not go through");

        assert_eq!(err.to_string(), "no space left");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "the old secret, intact\n"
        );
    }

    /// Where there was no file, a failed write leaves none: an empty lockfile
    /// is refused for having no version, an absent one is the ordinary start.
    #[test]
    fn a_private_write_that_fails_on_a_new_file_leaves_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.lock.toml");

        overwrite_with(&path, "secret\n", restrict_to_owner, |_, _| {
            Err(io::Error::other("refused"))
        })
        .expect_err("the write did not go through");

        assert!(left_in(dir.path()).is_empty(), "{:?}", left_in(dir.path()));
    }

    /// The same holds for a step before the write: a filesystem that refuses
    /// the restriction must not leave an empty file where there was none.
    #[test]
    fn a_private_write_refused_before_the_write_leaves_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.lock.toml");

        let err = overwrite_with(
            &path,
            "secret\n",
            |_, _, _| Err(io::Error::other("chmod refused")),
            put,
        )
        .expect_err("the restriction did not go through");

        assert_eq!(err.to_string(), "chmod refused");
        assert!(left_in(dir.path()).is_empty(), "{:?}", left_in(dir.path()));
    }

    /// A file that was already there is kept when the restriction fails: it
    /// was never touched.
    #[test]
    fn a_private_write_refused_before_the_write_keeps_a_file_that_was_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.lock.toml");
        std::fs::write(&path, "the old secret\n").expect("seed");

        overwrite_with(
            &path,
            "secret\n",
            |_, _, _| Err(io::Error::other("chmod refused")),
            put,
        )
        .expect_err("the restriction did not go through");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "the old secret\n"
        );
    }

    /// When the undo fails too, the error says the file may now be neither,
    /// and keeps what stopped the write as its cause.
    #[test]
    fn a_private_write_whose_undo_fails_says_the_file_may_be_inconsistent() {
        use std::io::{Seek, SeekFrom, Write};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.lock.toml");
        std::fs::write(&path, "the old secret\n").expect("seed");

        let err = overwrite_with(&path, "a new secret\n", restrict_to_owner, |file, bytes| {
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&bytes[..4])?;
            // The handle the undo gets can no longer write.
            *file = std::fs::File::open(&path)?;
            Err(io::Error::other("no space left"))
        })
        .expect_err("the write did not go through");

        let message = err.to_string();
        assert!(message.contains("could not be put back"), "{message}");
        assert!(message.contains("inconsistent"), "{message}");
        let cause = std::error::Error::source(&err).map(ToString::to_string);
        assert_eq!(cause.as_deref(), Some("no space left"));
    }

    /// The staged copy sits beside the file, so the rename never crosses a
    /// volume, which is the one case where a rename stops being atomic.
    #[test]
    fn the_staged_copy_sits_beside_the_file() {
        let sibling = sibling_of(Path::new("some/dir/fake.lock.toml"));

        assert_eq!(sibling.parent(), Some(Path::new("some/dir")));
        let name = sibling.file_name().expect("a name").to_string_lossy();
        assert!(name.starts_with(".fake.lock.toml."), "{name}");
        assert!(name.ends_with(".tmp"), "{name}");
    }

    /// A lockfile somebody tightened by hand stays tightened: the new file
    /// takes the mode of the one it replaces.
    #[cfg(unix)]
    #[test]
    fn write_keeps_the_mode_of_the_file_it_replaces() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = write("version = 1\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");

        super::write(&path, "version = 2\n").expect("write");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o640, "mode was {mode:o}");
    }

    /// Renaming over a symlink would swap the link for a regular file. The
    /// file written is the one the link points at, and the link stays.
    #[cfg(unix)]
    #[test]
    fn a_write_through_a_symlink_replaces_the_file_it_points_at() {
        let (dir, real) = write("version = 1\n");
        let link = dir.path().join("link.lock.toml");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        super::write(&link, "version = 2\n").expect("write");

        assert!(link.is_symlink(), "the link is still a link");
        assert_eq!(
            std::fs::read_to_string(&real).expect("read"),
            "version = 2\n"
        );
    }

    #[test]
    fn a_write_that_fails_for_another_reason_is_not_retried() {
        // ENOENT on Unix, ERROR_FILE_NOT_FOUND on Windows: never transient.
        let (result, attempts, waits) = write_failing_with(&[2]);
        let msg = format!("{:#}", result.expect_err("must fail"));
        assert!(msg.contains("Failed to write fake.lock.toml"), "{msg}");
        assert_eq!(attempts, 1);
        assert!(waits.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn a_file_held_by_a_reader_is_written_once_the_reader_lets_go() {
        let (result, attempts, waits) = write_failing_with(&[1224, 32]);
        result.expect("the third attempt succeeds");
        assert_eq!(attempts, 3);
        assert_eq!(waits, [WRITE_BACKOFF, WRITE_BACKOFF * 2]);
    }

    #[cfg(windows)]
    #[test]
    fn a_reader_that_never_lets_go_still_fails_after_the_last_attempt() {
        let (result, attempts, _) = write_failing_with(&[1224; 10]);
        let msg = format!("{:#}", result.expect_err("must give up"));
        assert!(msg.contains("Failed to write fake.lock.toml"), "{msg}");
        assert_eq!(attempts, WRITE_ATTEMPTS as usize);
    }

    #[test]
    fn validate_registry_rejects_duplicate_and_out_of_range_steps() {
        static DUPES: &[LockfileMigration] = &[V1_TO_V2, V1_TO_V2];
        assert!(format(3, DUPES).validate_registry().is_err());

        static PAST_CURRENT: &[LockfileMigration] = &[V1_TO_V2];
        assert!(format(1, PAST_CURRENT).validate_registry().is_err());

        assert!(format(2, &[V1_TO_V2]).validate_registry().is_ok());
    }
}
