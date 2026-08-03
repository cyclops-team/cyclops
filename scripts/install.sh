#!/bin/sh
#
# Cyclops installer. Builds both binaries, puts them somewhere your shell
# looks, and sets up the home directory. One command, then `cyclops start`.
#
#   ./scripts/install.sh              from a clone
#   ./scripts/install.sh --uninstall  take it back off
#
# Flags:
#   --prefix DIR   install the binaries here instead of picking a directory
#   --no-path      never edit a shell profile; print the line to add instead
#   --uninstall    remove the binaries and the profile block, keep ~/.cyclops
#   --help
#
# What it will and will not do: it never uses sudo, never touches your
# tmux config, and never edits a shell profile without printing the exact
# lines and where the backup went. Everything it writes is under your home.
#
# POSIX sh on purpose. It runs before cyclops exists on the machine, so it
# cannot assume anything more than the system shell.

set -eu

REPO_URL="${CYCLOPS_REPO:-https://github.com/cyclops-team/cyclops.git}"
REF="${CYCLOPS_REF:-main}"

# The block this script owns inside a shell profile. Matched literally on
# both install and uninstall, which is what makes a second run a no-op
# instead of a second copy.
MARK_START="# >>> cyclops >>>"
MARK_END="# <<< cyclops <<<"

PREFIX=""
NO_PATH=0
UNINSTALL=0

# ---------------------------------------------------------------------------
# output
# ---------------------------------------------------------------------------

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    DIM=$(printf '\033[2m') ; BOLD=$(printf '\033[1m') ; OFF=$(printf '\033[0m')
else
    DIM="" ; BOLD="" ; OFF=""
fi

say()  { printf '%s\n' "$*"; }
note() { printf '  %s%s%s\n' "$DIM" "$*" "$OFF"; }
step() { printf '\n%s==%s %s\n' "$DIM" "$OFF" "$*"; }

# Failures print what went wrong and the one command that fixes it. An
# installer that says "failed" and stops is where a first run dies.
die() {
    printf '\n%sinstall failed:%s %s\n' "$BOLD" "$OFF" "$1" >&2
    [ $# -gt 1 ] && printf '  %s\n' "$2" >&2
    exit 1
}

have() { command -v "$1" >/dev/null 2>&1; }

usage() {
    sed -n '3,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}

# ---------------------------------------------------------------------------
# arguments
# ---------------------------------------------------------------------------

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)    [ $# -ge 2 ] || die "--prefix needs a directory"; PREFIX="$2"; shift 2 ;;
        --prefix=*)  PREFIX="${1#--prefix=}"; shift ;;
        --no-path)   NO_PATH=1; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help)   usage ;;
        *)           die "unknown option: $1" "./scripts/install.sh --help lists them" ;;
    esac
done

# ---------------------------------------------------------------------------
# where things go
# ---------------------------------------------------------------------------

on_path() {
    case ":${PATH}:" in
        *":$1:"*) return 0 ;;
    esac
    return 1
}

# Pick a bin directory, preferring one your shell already searches. A
# directory already on PATH means no profile edit at all, which is the
# quietest install there is.
pick_prefix() {
    for d in "$HOME/.local/bin" "$HOME/bin" "$HOME/.cargo/bin"; do
        if on_path "$d"; then
            printf '%s\n' "$d"
            return
        fi
    done
    # Nothing on PATH to use. ~/.local/bin is the conventional answer and
    # gets one profile line below.
    printf '%s\n' "$HOME/.local/bin"
}

# The file that sets PATH for new shells of whatever the operator runs.
# Empty means an unrecognized shell, and then this script prints the line
# rather than guessing at a file.
profile_for_shell() {
    case "$(basename "${SHELL:-}")" in
        zsh)  printf '%s\n' "${ZDOTDIR:-$HOME}/.zshrc" ;;
        bash)
            # macOS Terminal starts login shells, which read .bash_profile
            # and not .bashrc. Prefer whichever is already there.
            if [ -f "$HOME/.bash_profile" ]; then
                printf '%s\n' "$HOME/.bash_profile"
            else
                printf '%s\n' "$HOME/.bashrc"
            fi
            ;;
        fish) printf '%s\n' "$HOME/.config/fish/config.fish" ;;
        *)    printf '\n' ;;
    esac
}

# The PATH line, in the syntax of the shell whose profile it lands in.
path_line() {
    case "$(basename "${SHELL:-}")" in
        fish) printf 'fish_add_path %s\n' "$1" ;;
        *)    printf 'export PATH="%s:$PATH"\n' "$1" ;;
    esac
}

# ---------------------------------------------------------------------------
# uninstall
# ---------------------------------------------------------------------------

# Take the marked block back out of a profile, byte for byte. Backs the
# file up first and prints where, because this edits a file the operator
# owns and did not write.
strip_block() {
    profile="$1"
    [ -f "$profile" ] || return 0
    grep -Fq "$MARK_START" "$profile" || return 0

    backup="$profile.cyclops-backup.$(date +%Y%m%d%H%M%S)"
    cp -p "$profile" "$backup"
    # Buffered so the blank line the install wrote above the block goes
    # with it. Unbuffered, an install/uninstall cycle leaves one behind
    # every time.
    tmp="$profile.cyclops-tmp.$$"
    awk -v s="$MARK_START" -v e="$MARK_END" '
        $0 == s { if (n > 0 && out[n] == "") n--; skip = 1; next }
        $0 == e { skip = 0; next }
        !skip   { out[++n] = $0 }
        END     { for (i = 1; i <= n; i++) print out[i] }
    ' "$profile" > "$tmp"
    mv -f "$tmp" "$profile"
    note "removed the cyclops block from $profile"
    note "the file as it was: $backup"
}

do_uninstall() {
    step "removing cyclops"
    removed=0
    for name in cyclops cyclopsd; do
        # An explicit --prefix wins; otherwise take whatever the shell
        # actually resolves, which is the copy that has been running.
        if [ -n "$PREFIX" ]; then
            bin="$PREFIX/$name"
        else
            bin="$(command -v "$name" 2>/dev/null || true)"
        fi
        if [ -n "$bin" ] && [ -f "$bin" ]; then
            rm -f "$bin"
            note "removed $bin"
            removed=$((removed + 1))
        fi
    done
    [ "$removed" -eq 0 ] && note "no cyclops binaries found on PATH"

    strip_block "$(profile_for_shell)"

    say ""
    say "${BOLD}✔ cyclops is uninstalled${OFF}"
    note "your record and config are untouched at ${CYCLOPS_HOME:-$HOME/.cyclops}"
    note "remove those too with: rm -rf ${CYCLOPS_HOME:-$HOME/.cyclops}"
    exit 0
}

[ "$UNINSTALL" -eq 1 ] && do_uninstall

# ---------------------------------------------------------------------------
# 1. requirements
# ---------------------------------------------------------------------------

step "checking requirements"

# tmux is not optional and not a runtime detail: every pane cyclops watches
# is a tmux pane. Missing or too old is a dead end later, so it stops here
# with the command that fixes it.
if ! have tmux; then
    if [ "$(uname -s)" = "Darwin" ]; then
        die "tmux is not installed" "brew install tmux"
    fi
    die "tmux is not installed" "sudo apt install tmux   (or your package manager's equivalent)"
fi
tmux_version="$(tmux -V 2>/dev/null | awk '{print $2}')"
tmux_major="$(printf '%s' "$tmux_version" | sed 's/[^0-9.].*//' | cut -d. -f1)"
tmux_minor="$(printf '%s' "$tmux_version" | sed 's/[^0-9.].*//' | cut -d. -f2)"
if [ -n "$tmux_major" ] && [ "$tmux_major" -lt 3 ] 2>/dev/null; then
    die "tmux $tmux_version is too old; cyclops needs 3.2 or newer" "upgrade tmux, then run this again"
elif [ "$tmux_major" = "3" ] && [ -n "$tmux_minor" ] && [ "$tmux_minor" -lt 2 ] 2>/dev/null; then
    die "tmux $tmux_version is too old; cyclops needs 3.2 or newer" "upgrade tmux, then run this again"
fi
note "tmux $tmux_version"

if ! have cargo; then
    die "cargo is not installed; cyclops builds from source" \
        "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi
note "cargo $(cargo --version 2>/dev/null | awk '{print $2}')"

# ---------------------------------------------------------------------------
# 2. the source
# ---------------------------------------------------------------------------

# Run from a clone, build that clone. Piped from the network with no clone
# around it, fetch one. Both end with SRC pointing at a workspace root.
SRC=""
case "$0" in
    */install.sh)
        candidate="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
        [ -f "$candidate/Cargo.toml" ] && SRC="$candidate"
        ;;
esac

CLONED=""
if [ -z "$SRC" ]; then
    have git || die "git is not installed, and there is no clone to build from" \
        "install git, or clone the repo and run ./scripts/install.sh inside it"
    step "fetching the source"
    CLONED="$(mktemp -d "${TMPDIR:-/tmp}/cyclops-install.XXXXXX")"
    trap 'rm -rf "$CLONED"' EXIT INT TERM
    git clone --depth 1 --branch "$REF" "$REPO_URL" "$CLONED/cyclops" >/dev/null 2>&1 ||
        die "could not clone $REPO_URL at $REF" "check the network, or set CYCLOPS_REF to a branch that exists"
    SRC="$CLONED/cyclops"
    note "$REPO_URL at $REF"
else
    note "building the clone at $SRC"
fi

# ---------------------------------------------------------------------------
# 3. build
# ---------------------------------------------------------------------------

step "building cyclops and cyclopsd"
say "${DIM}  a first build takes a few minutes${OFF}"
( cd "$SRC" && cargo build --release ) || die "the build failed" "the cargo output above says why"

TARGET="${CARGO_TARGET_DIR:-$SRC/target}/release"
for name in cyclops cyclopsd; do
    [ -x "$TARGET/$name" ] || die "the build finished but $TARGET/$name is missing"
done

# ---------------------------------------------------------------------------
# 4. install the binaries
# ---------------------------------------------------------------------------

[ -n "$PREFIX" ] || PREFIX="$(pick_prefix)"
step "installing to $PREFIX"

mkdir -p "$PREFIX" 2>/dev/null ||
    die "cannot create $PREFIX" "pick a directory you own: ./scripts/install.sh --prefix ~/.local/bin"
[ -w "$PREFIX" ] ||
    die "$PREFIX is not writable, and this installer never uses sudo" \
        "pick a directory you own: ./scripts/install.sh --prefix ~/.local/bin"

for name in cyclops cyclopsd; do
    # Copy beside the target and rename over it. Overwriting a running
    # binary in place fails with "text file busy"; a rename never does.
    cp "$TARGET/$name" "$PREFIX/$name.new"
    chmod +x "$PREFIX/$name.new"
    mv -f "$PREFIX/$name.new" "$PREFIX/$name"
    note "$PREFIX/$name"
done

# ---------------------------------------------------------------------------
# 5. PATH
# ---------------------------------------------------------------------------

# Three outcomes, and the last step of this script differs for each:
#
#   ok      this shell already finds $PREFIX; there is nothing to do
#   reload  the profile has the PATH line, this shell predates it
#   manual  nothing was edited, and the operator has a line to add
#
# "reload" covers a second run as well as a first: the block being there
# already says nothing about whether the shell running this has read it.
PATH_STATE=ok
PROFILE=""

if on_path "$PREFIX"; then
    note "$PREFIX is already on your PATH"
elif [ "$NO_PATH" -eq 1 ]; then
    PATH_STATE=manual
    say ""
    say "  $PREFIX is not on your PATH, and --no-path means this is yours to add:"
    say ""
    say "    $(path_line "$PREFIX")"
else
    PROFILE="$(profile_for_shell)"
    if [ -z "$PROFILE" ]; then
        # An unrecognized shell gets the line and no edit. Guessing at a
        # file for a shell this script does not know is how a profile
        # ends up with a line that never runs.
        PATH_STATE=manual
        say ""
        say "  cyclops does not know ${SHELL:-your shell}, so add this yourself:"
        say ""
        say "    $(path_line "$PREFIX")"
    elif [ -f "$PROFILE" ] && grep -Fq "$MARK_START" "$PROFILE"; then
        PATH_STATE=reload
        note "$PROFILE already has the cyclops block"
    else
        step "adding $PREFIX to your PATH"
        mkdir -p "$(dirname "$PROFILE")"
        if [ -f "$PROFILE" ]; then
            backup="$PROFILE.cyclops-backup.$(date +%Y%m%d%H%M%S)"
            cp -p "$PROFILE" "$backup"
        else
            backup=""
            : > "$PROFILE"
        fi
        {
            printf '\n%s\n' "$MARK_START"
            path_line "$PREFIX"
            printf '%s\n' "$MARK_END"
        } >> "$PROFILE"
        PATH_STATE=reload

        say "  three lines added to $PROFILE:"
        say ""
        printf '    %s\n' "$MARK_START"
        printf '    %s\n' "$(path_line "$PREFIX")"
        printf '    %s\n' "$MARK_END"
        say ""
        if [ -n "$backup" ]; then
            note "the file as it was: $backup"
            note "undo: cp \"$backup\" \"$PROFILE\"    (or ./scripts/install.sh --uninstall)"
        else
            note "undo: ./scripts/install.sh --uninstall"
        fi
    fi
    # This shell needs it too, so the setup and the checks below run
    # against the copy just installed.
    PATH="$PREFIX:$PATH"
    export PATH
fi

# ---------------------------------------------------------------------------
# 6. set up the home directory
# ---------------------------------------------------------------------------

# The config that says which tmux session to watch, and the detection
# manifests that let a pane be recognized at all. The binary owns this,
# not the installer: `cyclops start` writes exactly the same files, so
# there is one implementation of what a usable home is.
step "setting up ${CYCLOPS_HOME:-$HOME/.cyclops}"
"$PREFIX/cyclops" start --setup-only ||
    die "could not set up ${CYCLOPS_HOME:-$HOME/.cyclops}" "the output above says which file"

# ---------------------------------------------------------------------------
# 7. prove it works
# ---------------------------------------------------------------------------

version="$("$PREFIX/cyclops" --version 2>/dev/null || true)"
[ -n "$version" ] || die "$PREFIX/cyclops does not run" "try running it directly to see the error"

# What the shell resolves, which is the thing that actually matters. A
# binary on disk that the shell cannot find is not installed.
resolved="$(command -v cyclops 2>/dev/null || true)"

say ""
say "${BOLD}✔ $version is installed${OFF}"
note "cyclops    $PREFIX/cyclops"
note "cyclopsd   $PREFIX/cyclopsd"
note "home       ${CYCLOPS_HOME:-$HOME/.cyclops}"

say ""
say "Next:"
# The steps, padded to the widest command so the reasons line up the way
# `cyclops start` prints its own.
first=""
case "$PATH_STATE" in
    reload) first="exec ${SHELL:-sh} -l" ;;
    manual) first="add the PATH line above to your shell profile" ;;
esac
# The daemon comes before the workspace on purpose. Started second, it
# has no session to attach to and cyclops start has nothing to register a
# name with, so the first run names nothing and takes a second run to
# finish. Started first, one `cyclops start` does the whole thing.
daemon="cyclopsd &"
open="cyclops start"
width=${#open}
[ ${#daemon} -gt "$width" ] && width=${#daemon}
[ -n "$first" ] && [ ${#first} -gt "$width" ] && width=${#first}
n=1
if [ -n "$first" ]; then
    printf '  %s  %-*s  %s\n' "$n" "$width" "$first" "so your shell can find cyclops"
    n=$((n + 1))
fi
printf '  %s  %-*s  %s\n' "$n" "$width" "$daemon" "start the daemon"
n=$((n + 1))
printf '  %s  %-*s  %s\n' "$n" "$width" "$open" "open your workspace; it prints what to do next"

if [ "$PATH_STATE" = ok ] && [ -n "$resolved" ] && [ "$resolved" != "$PREFIX/cyclops" ]; then
    # Another cyclops is earlier on PATH. Saying "installed" without
    # saying this would leave the operator running a different binary
    # than the one this script just built.
    say ""
    say "  Heads up: your shell finds $resolved first, not the copy above."
    say "  Remove that one, or put $PREFIX earlier on your PATH."
fi
