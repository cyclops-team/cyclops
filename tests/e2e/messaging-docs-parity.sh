#!/usr/bin/env bash
# Reject known stale messaging language and the repository documentation style
# violation. Behavioral parity belongs to parity-check.sh and source tests.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
DOC_FILES=("$REPO/README.md")
while IFS= read -r file; do
  DOC_FILES+=("$file")
done < <(find "$REPO/docs" "$REPO/skills/cyclops" -type f \
  \( -name '*.md' -o -name '*.mdx' \) -print | sort)

reject() {
  local pattern="$1"
  if grep -Eni -- "$pattern" "${DOC_FILES[@]}"; then
    printf 'stale messaging or style language matched /%s/\n' "$pattern" >&2
    exit 1
  fi
}

# Standard send accepts a mailbox message. It does not paste the body or use
# legacy verified and unverified receipt tiers.
reject 'structured messages that wait for a safe moment before entering a pane'
reject 'cyclops waits until the pane is safe to inject'
reject 'hooks turn receipts from screen-verified into hook-verified'
reject 'delivers it with an evidence-labeled receipt'
reject 'any agent may query the whole record'

# Admin is a durable mailbox with no pane wake. Only a proven caller outside
# every watched pane has the admin identity.
reject 'admin inbox, which does not exist'
reject 'cyclops send admin.*(guaranteed|fails|no_such_target)'
reject 'every message you send from a terminal is from'

# The workspace journal owns mailbox facts. Session ledgers own pane state and
# legacy direct-delivery compatibility.
reject 'every message and state change lands in an append-only ledger'
reject 'canonical.*ledger/<session>'

# User-facing documentation follows the repository writing standard.
reject $'\u2014'

printf 'messaging stale-language and documentation style lint passed\n'
