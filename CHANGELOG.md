# Changelog

## [Unreleased]

## [0.1.3] - 2026-11-10

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
