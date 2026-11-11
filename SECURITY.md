# Security policy

`rbx-core` is the code the rbx-forge tools share for the parts that touch
credentials: the `--api-key` and `--cookie` flags, the opt-in Studio cookie,
the HTTP client that sends them, and the lockfiles written beside a project. A
defect here is a defect in every tool built on it.

## Reporting a vulnerability

Report it privately, through GitHub: on this repository, open the **Security**
tab and choose **Report a vulnerability**, or go straight to
<https://github.com/rbx-forge/rbx-core/security/advisories/new>.

Do not open a public issue for a vulnerability. Do not paste a real API key or
cookie into a report: a redacted reproduction is always enough.

What helps: the tag you depend on (or the tool and its `--version`), what you
ran, what you expected, what happened, and why it is a security problem rather
than a bug. A proof of concept is welcome but never required.

**What to expect.** This is a solo-maintained project. An acknowledgement
within a week is the realistic commitment; a fix lands as fast as the severity
warrants, as a new tag, and the tools move their pin to it. You will be
credited in the advisory unless you ask otherwise. There is no bounty program.

## Supported versions

Only the latest tag. At 0.x, fixes go out as a new tag rather than being
backported.

## What counts

In scope:

- A credential (API key, cookie) written to disk, logged, printed, or sent
  anywhere other than the Roblox host a request is meant for.
- The Studio cookie read without the consent prompt, or on a run that has
  nowhere to ask.
- A lockfile written readable by anyone other than its owner.
- A path traversal or arbitrary write from remote-controlled data.
- Dependency vulnerabilities with a plausible path to any of the above.

Out of scope:

- An API key doing what its scopes allow.
- Anything requiring an attacker who already has your credentials, or write
  access to your machine or repository.
- Rate limits, quotas, and Roblox-side API behavior. Report those to Roblox.
