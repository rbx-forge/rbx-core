# Changelog

## [Unreleased]

### Added

- `api::needs_scope` puts the scope a refused call needed on top of a 403, since Roblox answers `Scope not authorized` without naming it. Any other error passes through as it is.
- `newer`: the refusal a `sync` gives before it writes over a value changed on Roblox since the lockfile was written. `report` lists the values under their env and names both ways out (the pull, or `--overwrite-newer`), and `pull_hint` spells that pull for the run's target. The pull command is the caller's, binary included, so the hint names the tool that is running.

### Fixed

- `lockfile::write_private` removes a file it created when any step fails, not only the write: a filesystem that refuses the permission change, or a failed read, left an empty file behind, and an empty lockfile is refused for having no version where an absent one is the ordinary start.
- `lockfile::write_private` says so when the old content cannot be put back after a failed write: the error now reads that the file may be inconsistent, with the write's own error as its cause. That second failure was dropped, and the error read as if the file had been left as it was.
- On Windows, `lockfile::write_private` runs `icacls` once per file per process, and again only for a file the write created, instead of on every write and every retry. A tool that saves once per key no longer starts a process per key. A file that was already there is still restricted on its first write.
- `lockfile::write` reads the age of a file beside the lockfile only when its name is that of a staged copy. It read the metadata of every file in the directory, on every write.

## [0.1.3] - 2026-11-10

### Added

- `api::unconfirmed` attaches a caller's advice to a failed write that may still have been applied, and `api::may_have_landed` is the test it rests on: a 5xx, or a transport failure once the request has left. A refusal is an answer and a failure to connect sent nothing, so neither counts.

### Fixed

- `lockfile::write` replaces the file in one step: the content is staged in a sibling file, flushed, and renamed over it. A run that is killed partway, or a disk that fills, leaves the lockfile as it was. Writing in place truncated it first and could leave it empty, which reads as every id it tracked never having been created. A staged copy left by a killed run is removed by the next write.
- `lockfile::write_private` no longer empties the file before it writes. The content goes over the old one, the file is cut to length afterwards, and the old content is put back if a step fails, so a full disk leaves the file as it was. It is written in place on purpose: a staged copy would be a second file holding the secret, under a name the ignore rule for the first does not cover.

## [0.1.2] - 2026-11-09

### Added

- `THIRD-PARTY-NOTICES.md` carries the MIT notices of the Asphalt and Tarmac code that `alpha_bleed` adapts.

## [0.1.1] - 2026-11-08

### Fixed

- The hint after a missing scope names `rbx-apikey introspect` and `rbx-apikey resolve`. It still named `rbx apikey`, a subcommand that left rbx-deploy for a tool of its own.

## [0.1.0] - 2026-11-08

First release as a repository of its own. The crate lived inside the rbx-deploy workspace until now; its history came with it.

- Versioned on its own, at 0.1.0, and consumed by git tag instead of by the `main` branch of another repository.
- The two tests that walked the whole rbx-deploy workspace (binary names, environment variable names) stay with rbx-deploy.
