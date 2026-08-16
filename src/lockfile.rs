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
//!    names both versions and points at the real fix — upgrade — rather than
//!    surfacing a parse failure the user cannot act on.
//! 2. **Migrate older versions forward, one step at a time**, through
//!    [`LockfileFormat::migrations`]. Registries are keyed `"N -> N+1"` and
//!    applied in sequence until the file reaches the current version.
//! 3. **One helper**, so behaviour cannot drift between lockfiles.
//!
//! The migration registries are empty today, and that is the point: the
//! enforcement point has to exist *before* lockfiles are in the wild, because
//! it is the part that cannot be retrofitted cheaply afterwards. This mirrors
//! the project's own "an ignored key must not pass for applied" policy for
//! `rbx env` (see `docs/env.md`) — a version we silently ignore is a version we
//! silently claim to have honoured.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;

/// One step of a lockfile format migration.
///
/// `apply` receives the whole document as a mutable [`toml::Value`], before it
/// is deserialized into the concrete lockfile type — so a step is free to
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
    /// Returns `Ok(None)` when the file does not exist — a first run is not an
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

    fn write(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
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

    #[test]
    fn validate_registry_rejects_duplicate_and_out_of_range_steps() {
        static DUPES: &[LockfileMigration] = &[V1_TO_V2, V1_TO_V2];
        assert!(format(3, DUPES).validate_registry().is_err());

        static PAST_CURRENT: &[LockfileMigration] = &[V1_TO_V2];
        assert!(format(1, PAST_CURRENT).validate_registry().is_err());

        assert!(format(2, &[V1_TO_V2]).validate_registry().is_ok());
    }
}
