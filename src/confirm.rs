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

/// Prompt for the target's name typed back, rather than for a `y`.
///
/// For the operations where the cost of a slip is not the operation itself but
/// the fact that it landed on the wrong thing. A `y/N` confirms that you meant
/// to run the command; typing `PlayerData` back confirms that you meant to run
/// it **on `PlayerData`**, which is the mistake worth catching when the name
/// came from a shell history or an autocomplete.
///
/// `skip_yes` still bypasses it. `--yes` is what a script passes, and a script
/// that names the wrong store was going to name it in the typed answer too.
pub fn confirm_by_typing(name: &str, what: &str, skip_yes: bool) -> Result<()> {
    if skip_yes {
        return Ok(());
    }

    let typed = dialoguer::Input::<String>::new()
        .with_prompt(format!(
            "{} {what}\n  Type {} to confirm",
            "⚠".yellow(),
            name.bold()
        ))
        .allow_empty(true)
        .interact_text()
        .map_err(|e| anyhow::anyhow!("Prompt error: {}", e))?;

    // Exact, and deliberately not trimmed-and-lowercased. A near miss is a
    // reason to stop: the point of the question is that the answer had to be
    // read off the target rather than guessed at.
    if typed != name {
        bail!("Aborted: expected \"{name}\", got \"{typed}\".");
    }
    Ok(())
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

    /// The only branch reachable without a terminal, and the one a script
    /// takes. Everything else in `confirm_by_typing` needs somebody to type.
    #[test]
    fn typing_is_skipped_when_yes_set() {
        confirm_by_typing("PlayerData", "would prompt", true).expect("should skip");
    }
}
