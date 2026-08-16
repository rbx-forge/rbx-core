//! Centralized confirmation prompt for destructive operations.
//!
//! Two variants cover every existing call site:
//! - [`confirm_destructive`]: gated by a precomputed `required` bool (e.g.
//!   `env.confirm == true`). Skipped when `required == false` or when the
//!   user passed `--yes`.
//! - [`confirm_always`]: prompt unless `--yes`. For operations that are
//!   inherently destructive regardless of env (paid creates, --all rotations).

use anyhow::{bail, Result};
use colored::Colorize;
use dialoguer::Confirm;

/// Conditionally prompt. Returns `Ok(())` immediately when `required` is
/// false or `skip_yes` is true (the user passed `--yes`). Otherwise shows an
/// interactive y/N prompt and bails with "Aborted." on a negative answer.
pub fn confirm_destructive(prompt: &str, required: bool, skip_yes: bool) -> Result<()> {
    if !required || skip_yes {
        return Ok(());
    }
    prompt_user(prompt)
}

/// Always prompt unless `skip_yes` is true. Use for operations that are
/// inherently destructive regardless of the targeted environment
/// (paid creates, --all rotations, etc.).
pub fn confirm_always(prompt: &str, skip_yes: bool) -> Result<()> {
    if skip_yes {
        return Ok(());
    }
    prompt_user(prompt)
}

fn prompt_user(prompt: &str) -> Result<()> {
    let confirmed = Confirm::new()
        .with_prompt(format!("{} {}", "⚠".yellow(), prompt))
        .default(false)
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt error: {}", e))?;
    if !confirmed {
        bail!("Aborted.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_skips_when_not_required() {
        // required == false is the most common bypass path (env.confirm == false).
        confirm_destructive("would prompt", false, false).expect("should skip");
    }

    #[test]
    fn destructive_skips_when_yes_set() {
        // --yes always wins, even when the env requires confirmation.
        confirm_destructive("would prompt", true, true).expect("should skip");
    }

    #[test]
    fn destructive_skips_when_neither_required_nor_yes() {
        // Edge: both off → still skipped (the `!required` short-circuit wins).
        confirm_destructive("would prompt", false, true).expect("should skip");
    }

    #[test]
    fn always_skips_when_yes_set() {
        confirm_always("would prompt", true).expect("should skip");
    }
}
