#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ "${1:-}" == "--install-only" ]]; then
  shift
  [[ $# -eq 0 ]] || { echo "install.sh: --install-only takes no other options" >&2; exit 2; }
  exec "$SCRIPT_DIR/bin/commPact-install" install
fi

"$SCRIPT_DIR/bin/commPact-install" install
exec "${HOME:?HOME is required}/.commPact/bin/commPact-setup" --workdir "$PWD" "$@"
