#!/bin/sh
# Cyclops bootstrap installer.
#
#   curl -fsSL https://usecyclops.dev/install.sh | sh
#
# This script only fetches the release source and hands it to the release's
# own bin/commPact-install, which is what actually populates the install
# home. That installer never touches the network, never uses sudo, and never
# edits shell startup files or tmux.conf — this script preserves that by
# doing the one network fetch itself and nothing else privileged.
#
# Environment overrides:
#   CYCLOPS_REF          branch, tag, or (git only) commit SHA to install (default: main)
#   CYCLOPS_HOME          install destination (default: $HOME/.commPact)
#   CYCLOPS_BIN_DIR        where the `cyclops` command is linked (default: $HOME/.local/bin)
#   CYCLOPS_SOURCE_DIR    use this local directory instead of downloading (offline/dev use)
set -eu

REPO_OWNER="notyahir"
REPO_NAME="cyclops"
REF="${CYCLOPS_REF:-main}"
CYCLOPS_HOME="${CYCLOPS_HOME:-$HOME/.commPact}"
BIN_DIR="${CYCLOPS_BIN_DIR:-$HOME/.local/bin}"

say() { printf 'cyclops: %s\n' "$1"; }
err() { printf 'cyclops: %s\n' "$1" >&2; exit 1; }
need_cmd() { command -v "$1" >/dev/null 2>&1; }

[ -n "${HOME:-}" ] || err "HOME must be set"

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/cyclops-install.XXXXXX")"
cleanup() { rm -rf "$work_dir"; }
trap cleanup EXIT INT TERM

fetch_source() {
  src_dir="$work_dir/src"

  if [ -n "${CYCLOPS_SOURCE_DIR:-}" ]; then
    say "using local source: $CYCLOPS_SOURCE_DIR"
    cp -R "$CYCLOPS_SOURCE_DIR" "$src_dir"
    return
  fi

  if need_cmd git; then
    say "fetching $REPO_OWNER/$REPO_NAME@$REF (git)"
    git clone --depth 1 --branch "$REF" \
      "https://github.com/$REPO_OWNER/$REPO_NAME.git" "$src_dir" >/dev/null 2>&1 \
      || err "git clone failed; check your network connection and that ref '$REF' exists"
    return
  fi

  # Deliberately not refs/heads/$REF.tar.gz: that 404s for tags, and
  # CYCLOPS_REF is documented to accept either. GitHub's bare-ref archive
  # path resolves branches, tags, and full commit SHAs alike.
  archive_url="https://github.com/$REPO_OWNER/$REPO_NAME/archive/$REF.tar.gz"
  say "fetching $REPO_OWNER/$REPO_NAME@$REF (tarball)"
  if need_cmd curl; then
    curl -fsSL "$archive_url" -o "$work_dir/src.tar.gz" || err "download failed: $archive_url"
  elif need_cmd wget; then
    wget -q -O "$work_dir/src.tar.gz" "$archive_url" || err "download failed: $archive_url"
  else
    err "installing cyclops requires git, curl, or wget"
  fi
  mkdir -p "$src_dir"
  tar -xzf "$work_dir/src.tar.gz" -C "$src_dir" --strip-components=1 \
    || err "could not extract downloaded archive"
}

install_release() {
  installer="$src_dir/bin/commPact-install"
  [ -x "$installer" ] || err "downloaded release is missing bin/commPact-install"
  if [ -e "$CYCLOPS_HOME" ]; then
    say "updating existing install at $CYCLOPS_HOME"
    "$installer" update --destination "$CYCLOPS_HOME"
  else
    say "installing to $CYCLOPS_HOME"
    "$installer" install --destination "$CYCLOPS_HOME"
  fi
}

link_command() {
  [ -x "$CYCLOPS_HOME/bin/cyclops" ] || err "install completed but bin/cyclops is missing"
  mkdir -p "$BIN_DIR"

  target="$BIN_DIR/cyclops"
  if [ -e "$target" ] && [ ! -L "$target" ]; then
    err "$target already exists and isn't a symlink cyclops manages; move it aside (or set CYCLOPS_BIN_DIR to a different directory) and re-run"
  fi
  rm -f "$target"

  # A copy here (rather than a symlink) would break: bin/cyclops finds its
  # sibling commPact-* commands next to its real location, and a bare copy
  # in $BIN_DIR wouldn't have them. Fail clearly instead of shipping a
  # broken CLI on filesystems that don't support symlinks.
  ln -s "$CYCLOPS_HOME/bin/cyclops" "$target" \
    || err "could not create $target -> $CYCLOPS_HOME/bin/cyclops (symlinks unsupported here?); add $CYCLOPS_HOME/bin to your PATH instead"
}

path_has() {
  case ":$PATH:" in
    *":$1:"*) return 0 ;;
    *) return 1 ;;
  esac
}

main() {
  fetch_source
  install_release
  link_command

  say "installed: $BIN_DIR/cyclops"
  if path_has "$BIN_DIR"; then
    say "run: cyclops"
  else
    say ""
    say "$BIN_DIR is not on your PATH. Add it, then start a new shell:"
    say ""
    say "  export PATH=\"$BIN_DIR:\$PATH\""
    say ""
    say "then run: cyclops"
  fi
}

main
