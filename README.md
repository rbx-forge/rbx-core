# rbx-core

The library the rbx-forge command-line tools share: `rbxplace.toml` resolution, the global flags (`--api-key`, `--cookie`, `--env`, `--place`, `--universe-id`, `--place-id`), the Open Cloud HTTP client with its retry policy, the opt-in Studio cookie, and the write-or-check plumbing behind every `--check`.

It is not published on crates.io. Tools depend on it by git tag:

```toml
[dependencies]
rbx-core = { git = "https://github.com/rbx-forge/rbx-core", tag = "v0.1.0" }
```

## What is in it

| Module | Provides |
| --- | --- |
| `env` | `GlobalFlags`, a clap `Args` group to flatten into a tool's CLI; env, place and universe resolution; the cookie chain |
| `places` | `rbxplace.toml`: envs, places, groups, owner |
| `api` | The HTTP client builder, retry on 429, 5xx and transient network errors, CSRF handling, error types that keep the status |
| `session` | Checking a cookie's session before a write, and naming the account |
| `confirm` | Prompts that refuse cleanly without a terminal |
| `output` | One JSON document on stdout, notes on stderr |
| `lockfile` | Versioned lockfiles, written readable by the owner only |
| `newer` | The refusal a `sync` gives before it writes over a value changed on Roblox (`report`), and the pull it points at, spelled with the caller's own command (`pull_hint_for`) |
| `generated` | Write or `--check` a generated file against the same bytes |
| `image` | Alpha bleed before upload |

## Versioning

Tags follow semver at 0.x: a minor bump may break, a patch never does. Each tool pins a tag, so a change here reaches a tool only when that tool moves its pin.

## License

[MPL-2.0](./LICENSE).
