#!/bin/sh
#
# Cyclops installer. Builds both binaries, puts them somewhere your shell
# looks, and sets up the home directory. One command, then `cyclops`.
#
#   ./scripts/install.sh              from a clone
#   curl -fsSL https://www.usecyclops.dev/install.sh | sh
#   ./scripts/install.sh --uninstall  take it back off
#
# Flags:
#   --prefix DIR   install the binaries here instead of picking a directory
#   --no-path      never edit a shell profile; print the line to add instead
#   --uninstall    stop the daemon, remove the complete Cyclops state home,
#                  binaries, and the profile block
#   --help
#
# What it will and will not do: it never uses sudo, never touches your
# tmux config, and never edits a shell profile without printing the exact
# lines and where the backup went. Binaries go to the selected prefix;
# state and installed-agent integration stay under your home.
#
# POSIX sh on purpose. It runs before cyclops exists on the machine, so it
# cannot assume anything more than the system shell.

set -eu

REPO_URL="${CYCLOPS_REPO:-https://github.com/cyclops-team/cyclops.git}"
REF="${CYCLOPS_REF:-main}"

# Source installs must not hide an optimized Cargo build under macOS's
# per-process /private temporary directory. Keep every rebuildable installer
# artifact in the ordinary user cache instead. The source and private pair
# staging directories are removed after activation; Cargo's target directory
# remains for the next install or update.
installer_cache_root() {
    case "$(uname -s)" in
        Darwin) printf '%s\n' "$HOME/Library/Caches/Cyclops/installer" ;;
        *) printf '%s\n' "${XDG_CACHE_HOME:-$HOME/.cache}/cyclops/installer" ;;
    esac
}

INSTALLER_CACHE="$(installer_cache_root)"

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

# Pair activation is a commit point. Anything that fails after it must say
# what remains installed and how to finish, rather than implying no install
# landed.
incomplete() {
    printf '\n%sinstall committed; setup incomplete:%s %s\n' "$BOLD" "$OFF" "$1" >&2
    [ $# -gt 1 ] && printf '  %s\n' "$2" >&2
    exit 1
}

have() { command -v "$1" >/dev/null 2>&1; }

usage() {
    say "Cyclops installer. Builds cyclops and cyclopsd from source."
    say ""
    say "Usage:"
    say "  curl -fsSL https://www.usecyclops.dev/install.sh | sh"
    say "  ./scripts/install.sh [OPTIONS]"
    say ""
    say "Options:"
    say "  --prefix DIR   install the binaries in DIR"
    say "  --no-path      do not edit a shell profile"
    say "  --uninstall    stop the daemon, remove Cyclops state, binaries, and the PATH block"
    say "  --help         show this help"
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
        *)           die "unknown option: $1" "rerun the installer with --help to list its options" ;;
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

    # Establish one prefix from the explicit argument or the selected client.
    # The client validates both public names and stops only a daemon reporting
    # that exact selected executable before this shell removes anything.
    if [ -n "$PREFIX" ]; then
        pair_prefix="$PREFIX"
    else
        stopper="$(command -v cyclops 2>/dev/null || true)"
        pair_prefix=""
        [ -n "$stopper" ] && pair_prefix="$(dirname "$stopper")"
    fi
    if [ -z "$pair_prefix" ] && [ -n "$(command -v cyclopsd 2>/dev/null || true)" ]; then
        say "cannot identify one Cyclops install prefix because cyclops is not on PATH"
        say "nothing was removed; rerun with --prefix DIR naming the install to remove"
        exit 1
    fi
    if [ -z "$pair_prefix" ]; then
        note "no Cyclops installation found on PATH; nothing was removed"
        exit 0
    fi
    pair_prefix="$(CDPATH= cd "$pair_prefix" 2>/dev/null && pwd -P)" || {
        say "cannot resolve the selected Cyclops install prefix"
        say "nothing was removed"
        exit 1
    }
    stopper="$pair_prefix/cyclops"
    pair_root="$pair_prefix/.cyclops-pairs"
    if [ -z "$stopper" ] || [ ! -x "$stopper" ]; then
        say "cannot validate $pair_prefix without its installed cyclops binary"
        say "nothing was removed"
        exit 1
    fi
    had_pair_root=0
    { [ -e "$pair_root" ] || [ -L "$pair_root" ]; } && had_pair_root=1
    if ! "$stopper" update --stop-selected-daemon --prefix "$pair_prefix"; then
        say "refused to validate and stop the selected installation; nothing was removed"
        exit 1
    fi
    note "validated $pair_prefix and stopped its daemon if it was running"

    # `--uninstall` is an explicit request to remove Cyclops. Reuse the CLI's
    # complete-state remover after the exact installed daemon has stopped so
    # it keeps its private-root, lease, plan, and recovery protections. The
    # preview and confirmation occur back-to-back in this one process; a
    # changed state home still refuses rather than deleting unpreviewed data.
    state_preview="$("$stopper" remove --all)" || {
        say "could not preview the current Cyclops state home; binaries and PATH remain installed"
        exit 1
    }
    printf '%s\n' "$state_preview"
    state_confirmation="$(printf '%s\n' "$state_preview" | awk '
        /^  confirm     cyclops remove --all --confirm / {
            sub(/^  confirm     cyclops remove --all --confirm /, "")
            print
            exit
        }
    ')"
    if [ -n "$state_confirmation" ]; then
        if ! "$stopper" remove --all --confirm "$state_confirmation"; then
            say "complete Cyclops state removal did not finish; binaries and PATH remain installed"
            exit 1
        fi
    else
        case "$state_preview" in
            *"result      the current Cyclops state home is absent"*) ;;
            *)
                say "could not obtain the exact confirmation for the current Cyclops state home; binaries and PATH remain installed"
                exit 1
                ;;
        esac
    fi

    if ! "$stopper" update --remove-pair-store --prefix "$pair_prefix"; then
        say "Cyclops state was removed, but the validated binary pair could not be removed"
        exit 1
    fi
    if [ "$had_pair_root" -eq 1 ]; then
        note "removed $pair_root"
    fi

    removed=0
    for name in cyclops cyclopsd; do
        # Both public names come from the one prefix established by cyclops.
        # Resolving cyclopsd independently could delete a shadow installation.
        bin=""
        [ -n "$pair_prefix" ] && bin="$pair_prefix/$name"
        if [ -n "$bin" ] && { [ -f "$bin" ] || [ -L "$bin" ]; }; then
            rm -f "$bin"
            note "removed $bin"
            removed=$((removed + 1))
        fi
    done
    if [ "$removed" -eq 0 ]; then
        if [ -n "$pair_prefix" ]; then
            note "no Cyclops binaries found in $pair_prefix"
        else
            note "no Cyclops installation found on PATH"
        fi
    fi

    strip_block "$(profile_for_shell)"

    say ""
    say "${BOLD}✔ cyclops is uninstalled${OFF}"
    note "removed the complete Cyclops state home at ${CYCLOPS_HOME:-$HOME/.cyclops}"
    note "vendor hook configuration and skills in agent-owned directories remain outside this removal"
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

# Cyclops builds from source, so a machine without cargo cannot finish.
# Rather than stopping to hand the operator the rustup command, run it:
# rustup's own installer is non-interactive with -y, --no-modify-path
# keeps it out of shell profiles (this installer already manages PATH for
# its own prefix and does not want a second writer), and sourcing the env
# file puts cargo on PATH for this run only. CYCLOPS_NO_RUSTUP=1 declines
# and gets the old refusal with the command to run by hand.
if ! have cargo; then
    if [ -n "${CYCLOPS_NO_RUSTUP:-}" ]; then
        die "cargo is not installed; cyclops builds from source" \
            "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    fi
    step "cargo not found; installing Rust with rustup (CYCLOPS_NO_RUSTUP=1 declines)"
    if ! curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --default-toolchain stable; then
        die "the rustup install failed" \
            "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    fi
    # shellcheck disable=SC1091
    . "${CARGO_HOME:-$HOME/.cargo}/env"
    have cargo || die "rustup finished but cargo is still not on PATH" \
        "open a new shell and run this installer again"
    note "installed rust via rustup"
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
PAIR_SOURCE=""
cleanup() {
    [ -z "$PAIR_SOURCE" ] || rm -rf "$PAIR_SOURCE"
    [ -z "$CLONED" ] || rm -rf "$CLONED"
}
trap cleanup EXIT INT TERM

mkdir -p "$INSTALLER_CACHE" 2>/dev/null ||
    die "cannot create the Cyclops build cache at $INSTALLER_CACHE"
chmod 700 "$INSTALLER_CACHE" 2>/dev/null ||
    die "cannot secure the Cyclops build cache at $INSTALLER_CACHE"
note "build cache $INSTALLER_CACHE"

if [ -z "$SRC" ]; then
    have git || die "git is not installed, and there is no clone to build from" \
        "install git, or clone the repo and run ./scripts/install.sh inside it"
    step "fetching the source"
    CLONED="$(mktemp -d "$INSTALLER_CACHE/source.XXXXXX")"
    git clone --depth 1 --branch "$REF" "$REPO_URL" "$CLONED/cyclops" >/dev/null 2>&1 ||
        die "could not clone $REPO_URL at $REF" "check the network, or set CYCLOPS_REF to a branch that exists"
    SRC="$CLONED/cyclops"
    note "$REPO_URL at $REF"
else
    note "building the clone at $SRC"
fi

if [ -n "$CLONED" ]; then
    UNINSTALL_HINT='curl -fsSL https://www.usecyclops.dev/install.sh | sh -s -- --uninstall'
else
    UNINSTALL_HINT='./scripts/install.sh --uninstall'
fi

# ---------------------------------------------------------------------------
# 3. build
# ---------------------------------------------------------------------------

step "building cyclops and cyclopsd"
say "${DIM}  a first build takes a few minutes${OFF}"
# The dist profile is release without the thin-LTO link step, and the two
# named packages skip workspace members an install never runs. Both trims
# exist because this compile happens on every installing machine.
BUILD_TARGET="${CARGO_TARGET_DIR:-$INSTALLER_CACHE/target}"
( cd "$SRC" && CARGO_TARGET_DIR="$BUILD_TARGET" cargo build --profile dist -p cyclops -p cyclopsd ) || die "the build failed" "the cargo output above says why"

TARGET="$BUILD_TARGET/dist"
for name in cyclops cyclopsd; do
    [ -x "$TARGET/$name" ] || die "the build finished but $TARGET/$name is missing"
done

# Cargo may hard-link a top-level binary to its hashed build artifact. Copy
# both binaries into one private directory so the validator executes and
# stages the same unlinked candidate pair.
#
# F81 records a macOS fcopyfile failure after a successful Cargo build. The
# private destination is newly created and not public yet, so a verified byte
# copy is a safe fallback rather than a partial install.
copy_private_candidate() {
    if cp "$1" "$2" 2>/dev/null && cmp -s "$1" "$2"; then
        return 0
    fi
    rm -f "$2" || return 1
    if candidate_copy_error="$(dd if="$1" of="$2" bs=65536 2>&1)"; then
        if cmp -s "$1" "$2"; then
            return 0
        fi
        printf '%s\n' "private candidate copy did not preserve $1" >&2
    else
        printf '%s\n' "$candidate_copy_error" >&2
    fi
    return 1
}

PAIR_SOURCE="$(mktemp -d "$INSTALLER_CACHE/pair.XXXXXX")" ||
    die "cannot create a private candidate directory"
chmod 700 "$PAIR_SOURCE" || die "cannot secure the private candidate directory"
for name in cyclops cyclopsd; do
    copy_private_candidate "$TARGET/$name" "$PAIR_SOURCE/$name" ||
        die "cannot stage $name in the private candidate directory"
    chmod 755 "$PAIR_SOURCE/$name" ||
        die "cannot make the private $name candidate executable"
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

# Pair activation restarts an already-running daemon before setup below can
# replace its seeded manifests. Remember that pre-install fact so setup can
# reload only a daemon the operator was already running; a fresh install must
# not start one as a side effect.
DAEMON_WAS_RUNNING=0
if "$PAIR_SOURCE/cyclops" daemon status --json 2>/dev/null |
    grep -q '"daemon_process":'; then
    DAEMON_WAS_RUNNING=1
fi

# The candidate owns pair validation, journal replay, daemon quiescence, and
# the one selector change. The shell never publishes two binaries separately.
"$PAIR_SOURCE/cyclops" update --install-pair "$PAIR_SOURCE" --prefix "$PREFIX" ||
    die "the matched binary pair could not be activated" "the output above names the proof that failed"
note "$PREFIX/cyclops"
note "$PREFIX/cyclopsd"

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
        if ! mkdir -p "$(dirname "$PROFILE")"; then
            incomplete "cannot create the shell profile directory" \
                "the matched pair remains at $PREFIX; add this PATH line manually: $(path_line "$PREFIX")"
        fi
        if [ -f "$PROFILE" ]; then
            backup="$PROFILE.cyclops-backup.$(date +%Y%m%d%H%M%S)"
            if ! cp -p "$PROFILE" "$backup"; then
                incomplete "cannot back up $PROFILE" \
                    "the matched pair remains at $PREFIX; add this PATH line manually: $(path_line "$PREFIX")"
            fi
        else
            backup=""
            if ! : > "$PROFILE"; then
                incomplete "cannot create $PROFILE" \
                    "the matched pair remains at $PREFIX; add this PATH line manually: $(path_line "$PREFIX")"
            fi
        fi
        if ! {
            printf '\n%s\n' "$MARK_START"
            path_line "$PREFIX"
            printf '%s\n' "$MARK_END"
        } >> "$PROFILE"; then
            incomplete "cannot write the PATH block to $PROFILE" \
                "the matched pair remains at $PREFIX; restore the printed backup if needed, then add: $(path_line "$PREFIX")"
        fi
        PATH_STATE=reload

        say "  three lines added to $PROFILE:"
        say ""
        printf '    %s\n' "$MARK_START"
        printf '    %s\n' "$(path_line "$PREFIX")"
        printf '    %s\n' "$MARK_END"
        say ""
        if [ -n "$backup" ]; then
            note "the file as it was: $backup"
            note "undo: cp \"$backup\" \"$PROFILE\"    (or $UNINSTALL_HINT)"
        else
            note "undo: $UNINSTALL_HINT"
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
#
# --wire-hooks is the installer's to pass and nobody else's. Detecting an
# agent and hearing from it are different things: without hooks a pane is
# recognized but never reports a turn edge, and every delivery settles for
# screen evidence instead of the verified receipt. Wiring them is an edit
# to another tool's configuration, so it happens where a person has just
# asked for an install, and it merges around what is already there. Set
# CYCLOPS_NO_VENDOR_HOOKS=1 to install without it.
step "setting up ${CYCLOPS_HOME:-$HOME/.cyclops}"
if ! "$PREFIX/cyclops" start --setup-only --wire-hooks; then
    if [ "${CYCLOPS_UPDATE_DRIVER:-}" = 1 ]; then
        say ""
        say "${BOLD}! binary update committed; home setup still needs repair${OFF}"
        note "the active matched pair remains installed"
        note "repair: $PREFIX/cyclops start --setup-only --wire-hooks"
    else
        incomplete "home setup did not finish" \
            "repair it with: $PREFIX/cyclops start --setup-only --wire-hooks"
    fi
fi

# Activation above restarted the old daemon before setup installed this
# pair's manifests. Reload that same live service now so the selected binary
# and its detection rules become one effective version. No daemon was running
# on a fresh install, so that path remains setup-only.
if [ "$DAEMON_WAS_RUNNING" -eq 1 ]; then
    if ! "$PREFIX/cyclops" daemon restart --plain; then
        incomplete "the installed setup could not be loaded by the daemon" \
            "the matched pair remains active; retry: $PREFIX/cyclops daemon restart --plain"
    fi
fi

# ---------------------------------------------------------------------------
# 7. prove it works
# ---------------------------------------------------------------------------

version="$("$PREFIX/cyclops" --version 2>/dev/null || true)"
if [ -z "$version" ]; then
    if [ "${CYCLOPS_UPDATE_DRIVER:-}" = 1 ]; then
        say ""
        say "${BOLD}! binary update committed; installed CLI proof needs repair${OFF}"
        note "the active matched pair remains installed at $PREFIX"
        note "inspect: $PREFIX/cyclops --version"
        exit 0
    fi
    incomplete "installed CLI proof failed at $PREFIX/cyclops" \
        "the matched pair remains active; run $PREFIX/cyclops --version directly to inspect it"
fi

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
open="cyclops"
width=${#open}
[ -n "$first" ] && [ ${#first} -gt "$width" ] && width=${#first}
n=1
if [ -n "$first" ]; then
    printf '  %s  %-*s  %s\n' "$n" "$width" "$first" "so your shell can find cyclops"
    n=$((n + 1))
fi
printf '  %s  %-*s  %s\n' "$n" "$width" "$open" "open your workspace and start your agents"

if [ "$PATH_STATE" = ok ] && [ -n "$resolved" ] && [ "$resolved" != "$PREFIX/cyclops" ]; then
    # Another cyclops is earlier on PATH. Saying "installed" without
    # saying this would leave the operator running a different binary
    # than the one this script just built.
    say ""
    say "  Heads up: your shell finds $resolved first, not the copy above."
    say "  Remove that one, or put $PREFIX earlier on your PATH."
fi
