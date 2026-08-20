//! Shared infrastructure for the `rbx` toolkit. Each domain crate
//! (`rbx-shop`, `rbx-meta`, ...) consumes pieces of this crate to avoid
//! re-implementing places resolution, env target handling, the Open Cloud
//! HTTP client, and auth concerns.
//!
//! Public surface:
//! - [`GlobalFlags`] : the clap `Args` group embedded in the top-level
//!   `rbx` binary. Common to every subcommand. Cookie resolution
//!   (explicit, env var, or rbx_cookie auto-detect) lives on it directly.
//! - [`EnvTarget`] : `(env_name, universe_id, optional place_id)` resolved
//!   from `--env <name>` against `rbxplace.toml`, or from per-subcommand
//!   fallback (`[experience]` in a domain config).
//! - [`places`] : `rbxplace.toml` parsing.
//! - [`generated`] : write-or-check plumbing shared by the generators, so
//!   `--check` modes compare against the same bytes the writer would emit.
//! - [`lockfile`] : lockfile `version` validation and stepwise migration,
//!   shared so every lockfile refuses a future-version file the same way.
//! - [`output`] : the one place that serializes a command's result to stdout,
//!   so `--json` means the same thing everywhere.
//! - [`users`] : turn `156`, `@name` or a pasted profile link into a user id.
//!   Shared because every subcommand that acts on a player needs it.
//! - [`session`] : one `users/authenticated` call, cached per process, so a
//!   command that is about to write with the cookie finds out first that the
//!   session is over.
//! - [`universe`] : list a universe's places from the legacy `develop` host,
//!   which is the only place that enumeration exists. Needs no credential, so
//!   it works before a project has been configured.
//! - [`api`] : Open Cloud HTTP client + retry + asset download.

pub mod api;
pub mod confirm;
pub mod env;
pub mod fs_name;
pub mod generated;
pub mod image;
pub mod lockfile;
pub mod output;
pub mod owner;
pub mod places;
pub mod session;
pub mod templates;
pub mod universe;
pub mod users;

pub use env::{EnvTarget, GlobalFlags};
