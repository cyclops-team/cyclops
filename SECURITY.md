# Security

Cyclops is pre-release software (`v0.1.0`). It has not had a security
audit. Treat it accordingly: don't run it against panes or agents you don't
trust, and expect rough edges.

## Reporting a vulnerability

Report it privately, not in a public issue. Use GitHub's private
vulnerability reporting: on the repo's **Security** tab, click **Report a
vulnerability**.

If that form isn't available to you, open a plain issue asking a
maintainer to make contact — **without any exploit details, payloads, or
reproduction steps** in the issue itself. A maintainer will follow up to
move the details somewhere private.

## What not to put in a public issue

- Exploit code, payloads, or step-by-step reproduction of a vulnerability.
- Ledger contents, logs, tokens, or session output pulled from a real run
  — these can contain agent output or credentials you didn't intend to
  share.
- Anything that would let someone else reproduce the issue before a fix
  ships.

## What to expect

There's no formal SLA — this is a small pre-release project. A maintainer
will acknowledge the report and work with you on a fix and disclosure
timeline in good faith.
