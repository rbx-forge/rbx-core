# rbx-core

Shared infrastructure for the rbx toolkit: `rbxplace.toml` loader, `GlobalFlags` (clap Args group with `--api-key` / `--cookie` / `--env` / `--place` / `--places`), Open Cloud HTTP client with retry on 429/5xx and transient network errors, asset download helper, and the write-or-check plumbing the generators share so a `--check` compares against the same bytes the writer would emit. Every domain crate depends on this.

Part of [`rbx-forge/rbx-cli`](https://github.com/rbx-forge/rbx-cli). Not published as a standalone crate.
