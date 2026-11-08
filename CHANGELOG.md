# Changelog

## [Unreleased]

## [0.1.1] - 2026-11-08

### Fixed

- The hint after a missing scope names `rbx-apikey introspect` and `rbx-apikey resolve`. It still named `rbx apikey`, a subcommand that left rbx-deploy for a tool of its own.

## [0.1.0] - 2026-11-08

First release as a repository of its own. The crate lived inside the rbx-deploy workspace until now; its history came with it.

- Versioned on its own, at 0.1.0, and consumed by git tag instead of by the `main` branch of another repository.
- The two tests that walked the whole rbx-deploy workspace (binary names, environment variable names) stay with rbx-deploy.
