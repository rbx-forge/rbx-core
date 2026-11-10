# Changelog

## [Unreleased]

### Added

- `api::needs_scope` puts the scope a refused call needed on top of a 403, since Roblox answers `Scope not authorized` without naming it. Any other error passes through as it is.
- `newer`: the refusal a `sync` gives before it writes over a value changed on Roblox since the lockfile was written. `report` lists the values under their env and names both ways out (the pull, or `--overwrite-newer`), and `pull_hint_for` spells that pull for the run's target. When `--overwrite-newer` was passed, the line that says what is replaced goes to stderr, so a `--json` document on stdout stays whole. It takes the caller's whole pull command, binary included, so the hint names the tool that is running. The name differs from the `pull_hint` that took a bare subcommand (`"meta"`), so a call written for that one fails to compile instead of printing a command with no binary in it.

### Fixed

- `lockfile::write_private` removes a file it created when any step fails, not only the write: a filesystem that refuses the permission change, or a failed read, left an empty file behind, and an empty lockfile is refused for having no version where an absent one is the ordinary start.
- `lockfile::write_private` says so when the old content cannot be put back after a failed write: the error now reads that the file may be inconsistent, with the write's own error as its cause. That second failure was dropped, and the error read as if the file had been left as it was.
- On Windows, `lockfile::write_private` runs `icacls` once per file per process, and again only for a file the write created, instead of on every write and every retry. A tool that saves once per key no longer starts a process per key. A file that was already there is still restricted on its first write.
- `lockfile::write` reads the age of a file beside the lockfile only when its name is that of a staged copy. It read the metadata of every file in the directory, on every write.
- `api::execute_create_with_retry_policy` retries a failure to connect, which sent nothing, as it retries a 429. A timeout once connected, and any other transport failure after the request left, still come back on the first occurrence, as a write that may have landed.
- `api::may_have_landed`, and so `api::unconfirmed`, no longer take an answer turned into an error on the caller's side for a write that may have landed: a status from `error_for_status` (a 5xx still counts), a redirect the client's policy refused, a body that could not be decoded.
- `api::ConfigsClient::publish` and `overwrite_draft` are sent once after a 5xx or a timeout, which may hide a write that went through, and the error then says to check the revisions, or read the draft back, before running the command again. They were resent like a read. A 429 and a failure to connect are still retried.
- The doc of `api::execute_json` says its retries cover the send and the status only: the body read after them is not retried.
- A long wait before a retry says what Roblox answered: `Rate limited by Roblox` after a 429, `Roblox answered 503 Service Unavailable, retrying` after a 5xx. Every wait called itself a rate limit, and so did the refusal to wait when the server asks for too long. A status with no standard name, such as the 520 or 522 of a CDN, prints as its code alone rather than `<unknown status code>`.
- Messages that every tool shows name none: a lockfile from a newer version says to update the tool that wrote it, not `rbx`, and the warning for an unknown `rbxplace.toml` key says it was ignored by this build of rbx-core, whose version it prints. It said `rbx` with rbx-core's version, which matches no `rbx` release. The crate and module docs no longer describe the crate as part of one binary.
- The doc of `api::execute_with_retry_policy` says which requests it is for: reads, and writes whose second arrival the caller accepts, such as a `DELETE`. It still resends only after a timeout or a failure to connect, never after a connection that dropped once the request arrived.
- The doc of `lockfile::write` names where a rename cannot happen and the write fails: a directory the user cannot create files in, and a lockfile bind-mounted on its own into a container.
- The doc of `lockfile::write_private` says that a process killed during the write, or before the file is cut to length, can leave new and old content mixed, as a power cut can.

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
