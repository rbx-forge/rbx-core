//! Machine-readable output.
//!
//! One place serializes to stdout, so the rules that make piped output usable
//! are stated once instead of per command:
//!
//! - **stdout carries the document and nothing else.** Progress, warnings and
//!   errors go to stderr. A command that prints a status line to stdout under
//!   `--json` produces something `jq` cannot read, and the failure shows up in
//!   somebody's pipeline rather than in review.
//! - **One JSON document per invocation**, pretty-printed with a trailing
//!   newline. Pretty because these get read by people at least as often as by
//!   scripts, and `jq` does not care either way.
//! - **`--json` implies non-interactive.** A prompt on stdout would corrupt the
//!   document and a prompt on stderr would hang a pipeline; a command that has
//!   something to ask must fail instead. See [`OutputFormat::may_prompt`].
//! - **Objects, not positional arrays.** A consumer survives a field being
//!   added; it does not survive a column shifting.

use std::fmt::Display;
use std::io::{IsTerminal, Write};

use anyhow::{Context, Result};
use serde::Serialize;

/// Bumped when a documented field changes meaning or disappears. Emitted as
/// `schema_version` so a consumer can refuse a document it does not
/// understand rather than silently read a renamed field as missing.
///
/// Adding a field is not a bump: consumers are expected to ignore what they do
/// not know.
pub const SCHEMA_VERSION: u32 = 1;

/// How a command renders its result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Coloured, laid out for reading. The default, and unchanged by anything
    /// in this module.
    #[default]
    Human,
    /// One JSON document on stdout.
    Json,
}

impl OutputFormat {
    /// Pick the format from a `--json` flag.
    pub fn from_json_flag(json: bool) -> Self {
        if json {
            OutputFormat::Json
        } else {
            OutputFormat::Human
        }
    }

    /// True when stdout belongs to the document.
    ///
    /// Callers branch on this for two things, and both matter: they must not
    /// print human output, and they must not prompt. Anything that would have
    /// asked a question has to fail with a message on stderr instead.
    pub fn is_json(self) -> bool {
        matches!(self, OutputFormat::Json)
    }

    /// Whether this invocation may stop and ask a question.
    ///
    /// Two conditions, and both have to hold. There has to be a human on the
    /// other end (that is [`is_interactive`], the same test the pickers in
    /// `rbx-ops ads` and `rbx shop pull` already apply) and stdout has to be
    /// free, which it is not under `--json`.
    ///
    /// Callers that would have prompted use this to take their non-interactive
    /// branch: skip an optional question, or fail with a message on stderr
    /// naming the flag that answers it. What they must never do is prompt
    /// anyway; that is a pipeline hanging on a question nobody sees.
    pub fn may_prompt(self) -> bool {
        !self.is_json() && is_interactive()
    }

    /// Where a human-facing line has to go to be harmless.
    ///
    /// Exposed separately from [`OutputFormat::note`] so a test can assert the
    /// routing without capturing the process's own streams.
    pub fn note_stream(self) -> Stream {
        if self.is_json() {
            Stream::Stderr
        } else {
            Stream::Stdout
        }
    }

    /// Print a line that is worth showing but is not the result.
    ///
    /// "No servers matched", "reading rbxplace.toml", a count. Under `Human`
    /// this is stdout, byte for byte what the command printed before. Under
    /// `Json` the same line goes to stderr, because stdout carries the
    /// document. Warnings do not go through here: they belong on stderr in
    /// both formats, so they are plain `eprintln!` at the call site.
    pub fn note(self, line: impl Display) {
        match self.note_stream() {
            Stream::Stdout => println!("{line}"),
            Stream::Stderr => eprintln!("{line}"),
        }
    }
}

/// Which of the process's two streams a line lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// True when there is a human on the other end to ask.
///
/// stdin because that is where an answer would come from, stderr because that
/// is where the question is drawn. Redirect either and the command is being
/// scripted, whatever the format flag says.
///
/// This is the canonical copy. `rbx-ops ads` and `rbx shop pull` each carry a
/// private one predating it; they answer the same, and fold into this when
/// they are next touched.
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Write `value` to stdout as the invocation's JSON document.
pub fn emit<T: Serialize>(value: &T) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    write_json(&mut stdout, value)?;
    stdout.flush().context("Failed to flush stdout")
}

/// The same rendering, against any writer. Exists so a test can assert the
/// exact bytes a command emits without capturing the process's stdout.
pub fn write_json<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
    let rendered = serde_json::to_string_pretty(value).context("Failed to serialize output")?;
    writer
        .write_all(rendered.as_bytes())
        .and_then(|()| writer.write_all(b"\n"))
        .context("Failed to write output")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Sample {
        name: &'static str,
        count: u32,
        nested: Vec<&'static str>,
    }

    fn render(value: &impl Serialize) -> String {
        let mut buf = Vec::new();
        write_json(&mut buf, value).expect("write");
        String::from_utf8(buf).expect("utf-8")
    }

    #[test]
    fn a_document_is_pretty_printed_and_newline_terminated() {
        let out = render(&Sample {
            name: "check",
            count: 2,
            nested: vec!["a"],
        });

        assert!(out.ends_with("}\n"), "{out:?}");
        assert!(out.contains("\n  \"name\": \"check\""), "{out:?}");
    }

    /// A consumer that pipes several invocations together must be able to read
    /// each one back; the trailing newline is part of that contract.
    #[test]
    fn a_document_round_trips_through_serde_json() {
        let out = render(&Sample {
            name: "check",
            count: 2,
            nested: vec!["a", "b"],
        });

        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed["name"], "check");
        assert_eq!(parsed["count"], 2);
        assert_eq!(parsed["nested"][1], "b");
    }

    #[test]
    fn the_human_format_is_the_default_and_json_is_opt_in() {
        assert_eq!(OutputFormat::default(), OutputFormat::Human);
        assert!(!OutputFormat::default().is_json());
        assert!(OutputFormat::from_json_flag(true).is_json());
        assert!(!OutputFormat::from_json_flag(false).is_json());
    }

    /// The rule the whole module exists for: under `--json` nothing but the
    /// document may reach stdout, notes included.
    #[test]
    fn a_note_leaves_stdout_alone_under_json_and_keeps_it_under_human() {
        assert_eq!(OutputFormat::Json.note_stream(), Stream::Stderr);
        assert_eq!(OutputFormat::Human.note_stream(), Stream::Stdout);
    }

    /// `--json` refuses to prompt whether or not a TTY is attached, so this
    /// holds in a terminal and in CI alike. The `Human` side depends on the
    /// environment and is therefore only asserted against [`is_interactive`].
    #[test]
    fn json_never_prompts_and_human_defers_to_the_tty_test() {
        assert!(!OutputFormat::Json.may_prompt());
        assert_eq!(OutputFormat::Human.may_prompt(), is_interactive());
    }

    #[test]
    fn a_write_failure_is_reported_rather_than_swallowed() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let err = write_json(
            &mut Closed,
            &Sample {
                name: "x",
                count: 0,
                nested: Vec::new(),
            },
        )
        .expect_err("a broken pipe must surface");
        assert!(
            format!("{err:#}").contains("Failed to write output"),
            "{err:#}"
        );
    }
}
