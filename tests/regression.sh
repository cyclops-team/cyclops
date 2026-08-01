#!/usr/bin/env bash
set -euo pipefail

RELEASE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASH_BIN="$(command -v bash)"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/commPact-regression.XXXXXX")"
SOCKET="$TMP_ROOT/tmux.sock"
HOME_TEST="$TMP_ROOT/home"
DEST="$HOME_TEST/.commPact"
mkdir -p "$HOME_TEST" "$TMP_ROOT/fake"
trap 'tmux -S "$SOCKET" kill-server >/dev/null 2>&1 || true; rm -rf "$TMP_ROOT"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
assert_file() { [[ -f "$1" ]] || fail "missing file: $1"; }
assert_eq() { [[ "$1" == "$2" ]] || fail "expected '$2', got '$1'"; }

printf 'commPact regression: release tree\n'
for command_name in commPact commPact-adopt commPact-init commPact-layout commPact-msg commPact-notice commPact-install commPact-setup commPact-state-watchdog cyclops; do
  assert_file "$RELEASE/bin/$command_name"
  [[ -x "$RELEASE/bin/$command_name" ]] || fail "not executable: $command_name"
done
assert_file "$RELEASE/install.sh"
[[ -x "$RELEASE/install.sh" ]] || fail "install.sh is not executable"
assert_file "$RELEASE/LICENSE"
grep -q 'Copyright (c) 2026 shawn pana' "$RELEASE/LICENSE" || fail "upstream MIT copyright missing"
for release_file in .gitignore .gitattributes .github/workflows/ci.yml CHANGELOG.md docs/RELEASING.md; do
  assert_file "$RELEASE/$release_file"
done
grep -q '^config/team.conf$' "$RELEASE/.gitignore" || fail "generated team config is not ignored"
grep -q 'bash tests/regression.sh' "$RELEASE/.github/workflows/ci.yml" || fail "CI does not run the regression suite"

printf 'commPact regression: forced SHA backends\n'
assert_eq "$(COMMPACT_SHA256_BACKEND=openssl "$RELEASE/bin/commPact" hash abc)" "ba7816bf8f01"
if command -v shasum >/dev/null 2>&1; then
  assert_eq "$(COMMPACT_SHA256_BACKEND=shasum "$RELEASE/bin/commPact" hash abc)" "ba7816bf8f01"
fi
if command -v sha256sum >/dev/null 2>&1; then
  assert_eq "$(COMMPACT_SHA256_BACKEND=sha256sum "$RELEASE/bin/commPact" hash abc)" "ba7816bf8f01"
fi

printf 'commPact regression: no network, shell rc, or tmux.conf mutation\n'
printf 'sentinel\n' > "$HOME_TEST/.bashrc"
printf 'sentinel\n' > "$HOME_TEST/.zshrc"
printf 'sentinel\n' > "$HOME_TEST/.tmux.conf"
shasum -a 256 "$HOME_TEST/.bashrc" "$HOME_TEST/.zshrc" "$HOME_TEST/.tmux.conf" > "$TMP_ROOT/config.before"
for blocked in curl wget sudo brew apt-get; do
  printf '#!/usr/bin/env bash\nexit 91\n' > "$TMP_ROOT/fake/$blocked"
  chmod +x "$TMP_ROOT/fake/$blocked"
done
PATH="$TMP_ROOT/fake:$PATH" HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" help >/dev/null
shasum -a 256 "$HOME_TEST/.bashrc" "$HOME_TEST/.zshrc" "$HOME_TEST/.tmux.conf" > "$TMP_ROOT/config.after"
cmp "$TMP_ROOT/config.before" "$TMP_ROOT/config.after"
if PATH="$TMP_ROOT/fake" HOME="$HOME_TEST" "$BASH_BIN" "$RELEASE/bin/commPact-install" install --destination "$DEST" > "$TMP_ROOT/no-tmux.out" 2>&1; then
  fail "install continued without tmux"
fi
grep -q 'tmux is missing' "$TMP_ROOT/no-tmux.out" || fail "missing-tmux diagnostic absent"
[[ ! -e "$DEST" ]] || fail "missing-tmux check mutated destination"

printf 'commPact regression: install and command-name resolution\n'
HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" install >/dev/null
[[ ! -e "$DEST/config/team.conf" ]] || fail "fresh install shipped a concrete team configuration"
[[ ! -e "$DEST/.DS_Store" && ! -e "$DEST/.git" ]] || fail "fresh install copied local source artifacts"
assert_file "$DEST/CHANGELOG.md"
if HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" install --destination "$DEST" >/dev/null 2>&1; then
  fail "install replaced an existing home without --replace"
fi
HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" install --replace --destination "$DEST" >/dev/null
PATH="$DEST/bin:$PATH" HOME="$HOME_TEST" bash -c 'for name in commPact commPact-adopt commPact-init commPact-layout commPact-msg commPact-notice commPact-install commPact-setup commPact-state-watchdog cyclops; do command -v "$name" >/dev/null || exit 1; done; commPact-msg --help >/dev/null; commPact-setup --help >/dev/null; commPact-install version >/dev/null; cyclops help >/dev/null; cyclops version >/dev/null'
[[ ! -e "$DEST.next.$$" ]] || fail "staging path leaked"

printf 'commPact regression: update and retained backup\n'
printf 'old release marker\n' > "$DEST/old-marker"
cat > "$DEST/config/team.conf" <<'EOF'
version=1
session=persistent-team
workdir=/tmp
layout=tiled
operator=operator
default_target=lead
agent_roles=lead,worker
role=operator|sh
role=lead|sh
role=worker|sh
EOF
HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" update --destination "$DEST" >/dev/null
[[ ! -e "$DEST/old-marker" ]] || fail "update retained old content in active tree"
grep -q '^session=persistent-team$' "$DEST/config/team.conf" || fail "update did not preserve team configuration"
BACKUP="$(find "$TMP_ROOT" -maxdepth 4 -type f -name old-marker -print | sed 's#/old-marker$##' | head -n 1)"
[[ -n "$BACKUP" && -d "$BACKUP" ]] || fail "update backup missing"
assert_file "$BACKUP/old-marker"

printf 'commPact regression: isolated tmux init, metadata, layout, and sibling path\n'
tmux -S "$SOCKET" -f /dev/null new-session -d -s bootstrap sh
env -u TMUX -u TMUX_PANE COMMPACT_SOCKET="$SOCKET" "$DEST/bin/commPact-init" --config "$DEST/config/team.conf.example" >/dev/null
roles="$(tmux -S "$SOCKET" show-options -t team-example -v @commPact_roles)"
assert_eq "$roles" "operator,lead,worker,reviewer,observer"
operator="$(tmux -S "$SOCKET" show-options -t team-example -v @commPact_operator_role)"
assert_eq "$operator" "operator"
env -u TMUX -u TMUX_PANE COMMPACT_SOCKET="$SOCKET" "$DEST/bin/commPact-layout" --config "$DEST/config/team.conf.example" --theme-only >/dev/null
env -u TMUX -u TMUX_PANE COMMPACT_SOCKET="$SOCKET" "$DEST/bin/commPact-msg" --session team-example --help >/dev/null
raw_label="$(COMMPACT_SOCKET="$SOCKET" "$DEST/bin/commPact" resolve lead)"
at_label="$(COMMPACT_SOCKET="$SOCKET" "$DEST/bin/commPact" resolve @lead)"
assert_eq "$at_label" "$raw_label"

printf 'commPact regression: cyclops entry point\n'
cyclops_help="$("$DEST/bin/cyclops" help)"
[[ "$cyclops_help" == *'one team, any agent'* ]] || fail "cyclops help output missing banner"
cyclops_hash="$("$DEST/bin/cyclops" hash abc)"
assert_eq "$cyclops_hash" "ba7816bf8f01"
cyclops_resolve="$(COMMPACT_SOCKET="$SOCKET" "$DEST/bin/cyclops" resolve lead)"
assert_eq "$cyclops_resolve" "$raw_label"
cyclops_list="$(COMMPACT_SOCKET="$SOCKET" "$DEST/bin/cyclops" list)"
[[ -n "$cyclops_list" ]] || fail "cyclops list produced no output"

printf 'commPact regression: cyclops start_strategy resume/attach/setup decision\n'
NO_CONFIG="$TMP_ROOT/no-such-config.conf"
STUB_CONFIG="$TMP_ROOT/strategy-stub.conf"
: >"$STUB_CONFIG"
strategy_setup="$(
  CYCLOPS_TEST_SESSION="no-such-session" CYCLOPS_TEST_CONFIG="$NO_CONFIG" COMMPACT_SOCKET="$SOCKET" \
    bash -c 'source "$1"; start_strategy "$CYCLOPS_TEST_SESSION" "$CYCLOPS_TEST_CONFIG"' _ "$DEST/bin/cyclops"
)"
assert_eq "$strategy_setup" "setup"
strategy_init="$(
  CYCLOPS_TEST_SESSION="no-such-session" CYCLOPS_TEST_CONFIG="$STUB_CONFIG" COMMPACT_SOCKET="$SOCKET" \
    bash -c 'source "$1"; start_strategy "$CYCLOPS_TEST_SESSION" "$CYCLOPS_TEST_CONFIG"' _ "$DEST/bin/cyclops"
)"
assert_eq "$strategy_init" "init"
strategy_attach="$(
  CYCLOPS_TEST_SESSION="team-example" CYCLOPS_TEST_CONFIG="$STUB_CONFIG" COMMPACT_SOCKET="$SOCKET" \
    bash -c 'source "$1"; start_strategy "$CYCLOPS_TEST_SESSION" "$CYCLOPS_TEST_CONFIG"' _ "$DEST/bin/cyclops"
)"
assert_eq "$strategy_attach" "attach"

printf 'commPact regression: cyclops update/uninstall target their own install home\n'
CUSTOM_HOME="$TMP_ROOT/custom-cyclops-home"
DEST_CHANGELOG_BEFORE="$(cat "$DEST/CHANGELOG.md")"
HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" install --destination "$CUSTOM_HOME" >/dev/null
assert_file "$CUSTOM_HOME/bin/cyclops"
[[ "$(cat "$DEST/CHANGELOG.md")" == "$DEST_CHANGELOG_BEFORE" ]] \
  || fail "custom-destination install modified the default install home"

UPDATE_FIXTURE="$TMP_ROOT/update-fixture"
cp -R "$RELEASE" "$UPDATE_FIXTURE"
printf '\n### cyclops regression marker %s\n' "$$" >>"$UPDATE_FIXTURE/CHANGELOG.md"
CYCLOPS_SOURCE_DIR="$UPDATE_FIXTURE" HOME="$HOME_TEST" "$CUSTOM_HOME/bin/cyclops" update >/dev/null
grep -q "cyclops regression marker $$" "$CUSTOM_HOME/CHANGELOG.md" \
  || fail "cyclops update did not install the fetched release into its own install home"
grep -q "cyclops regression marker $$" "$DEST/CHANGELOG.md" \
  && fail "cyclops update touched the default install home instead of its own"

CUSTOM_BIN="$TMP_ROOT/custom-bin"
mkdir -p "$CUSTOM_BIN"
ln -s "$CUSTOM_HOME/bin/cyclops" "$CUSTOM_BIN/cyclops"
HOME="$HOME_TEST" "$CUSTOM_BIN/cyclops" uninstall >/dev/null
[[ ! -e "$CUSTOM_HOME" ]] || fail "cyclops uninstall did not remove its own install home"
[[ -e "$DEST" ]] || fail "cyclops uninstall removed the default install home instead of its own"
[[ ! -e "$CUSTOM_BIN/cyclops" && ! -L "$CUSTOM_BIN/cyclops" ]] || fail "cyclops uninstall left a dangling PATH symlink"

printf 'commPact regression: cyclops refuses to update/uninstall an unmanaged directory\n'
assert_file "$DEST/.commPact-installed"
UNMANAGED_CHECKOUT="$TMP_ROOT/unmanaged-checkout"
mkdir -p "$UNMANAGED_CHECKOUT/bin"
cp "$RELEASE/bin/cyclops" "$UNMANAGED_CHECKOUT/bin/cyclops"
chmod +x "$UNMANAGED_CHECKOUT/bin/cyclops"
[[ ! -e "$UNMANAGED_CHECKOUT/.commPact-installed" ]] || fail "unmanaged fixture unexpectedly carries the managed-install marker"
if HOME="$HOME_TEST" "$UNMANAGED_CHECKOUT/bin/cyclops" update >/dev/null 2>&1; then
  fail "cyclops update ran against a directory without a managed-install marker"
fi
[[ -d "$UNMANAGED_CHECKOUT" ]] || fail "cyclops update guard did not stop before touching the unmanaged directory"
if HOME="$HOME_TEST" "$UNMANAGED_CHECKOUT/bin/cyclops" uninstall >/dev/null 2>&1; then
  fail "cyclops uninstall deleted a directory without a managed-install marker"
fi
[[ -d "$UNMANAGED_CHECKOUT" ]] || fail "cyclops uninstall guard did not stop before deleting the unmanaged directory"

printf 'commPact regression: cyclops start --help and setup-only arg rejection\n'
cyclops_start_help="$("$DEST/bin/cyclops" start --help)"
[[ "$cyclops_start_help" == *'Usage: commPact-setup'* ]] || fail "cyclops start --help did not show setup usage"
REJECT_CONFIG="$TMP_ROOT/cyclops-reject-args.conf"
cat > "$REJECT_CONFIG" <<'EOF'
version=1
session=cyclops-reject-args-session
workdir=/tmp
layout=tiled
operator=operator
default_target=lead
agent_roles=lead
role=operator|sh
role=lead|sh
EOF
if COMMPACT_SOCKET="$SOCKET" "$DEST/bin/cyclops" start --config "$REJECT_CONFIG" --roles solo >/dev/null 2>&1; then
  fail "cyclops start silently accepted a setup-only flag once a config already exists"
fi

printf 'commPact regression: cyclops start rejects commPact-setup-only config modes\n'
for setup_only_flag in --dry-run --config-only --replace-config; do
  if "$DEST/bin/cyclops" start "$setup_only_flag" >/dev/null 2>&1; then
    fail "cyclops start accepted $setup_only_flag, which is commPact-setup only"
  fi
done
# The rejection is unconditional -- it must fire even when a workspace
# already exists (the resume path), not just on first-time setup.
if COMMPACT_SOCKET="$SOCKET" "$DEST/bin/cyclops" start --config "$REJECT_CONFIG" --config-only >/dev/null 2>&1; then
  fail "cyclops start accepted --config-only against an existing config"
fi

printf 'commPact regression: public installer script (offline via CYCLOPS_SOURCE_DIR)\n'
INSTALLER_HOME="$TMP_ROOT/installer-home"
INSTALLER_BIN="$TMP_ROOT/installer-bin"
env -u TMUX -u TMUX_PANE HOME="$HOME_TEST" CYCLOPS_SOURCE_DIR="$DEST" \
  CYCLOPS_HOME="$INSTALLER_HOME" CYCLOPS_BIN_DIR="$INSTALLER_BIN" \
  sh "$RELEASE/frontend/static/install.sh" >/dev/null
assert_file "$INSTALLER_HOME/.commPact-installed"
[[ -L "$INSTALLER_BIN/cyclops" ]] || fail "public installer did not link cyclops onto PATH"
[[ "$(readlink "$INSTALLER_BIN/cyclops")" == "$INSTALLER_HOME/bin/cyclops" ]] \
  || fail "public installer linked cyclops to the wrong target"

printf 'commPact regression: public installer refuses to hijack an unmanaged symlink\n'
HIJACK_BIN="$TMP_ROOT/installer-hijack-bin"
mkdir -p "$HIJACK_BIN" "$TMP_ROOT/unrelated-tool"
printf '#!/bin/sh\necho unrelated\n' > "$TMP_ROOT/unrelated-tool/cyclops"
chmod +x "$TMP_ROOT/unrelated-tool/cyclops"
ln -s "$TMP_ROOT/unrelated-tool/cyclops" "$HIJACK_BIN/cyclops"
if env -u TMUX -u TMUX_PANE HOME="$HOME_TEST" CYCLOPS_SOURCE_DIR="$DEST" \
  CYCLOPS_HOME="$INSTALLER_HOME" CYCLOPS_BIN_DIR="$HIJACK_BIN" \
  sh "$RELEASE/frontend/static/install.sh" >/dev/null 2>&1; then
  fail "public installer silently replaced a symlink it doesn't manage"
fi
[[ "$(readlink "$HIJACK_BIN/cyclops")" == "$TMP_ROOT/unrelated-tool/cyclops" ]] \
  || fail "public installer removed an unmanaged symlink before refusing to proceed"

printf 'commPact regression: cyclops update/uninstall take no options besides --help\n'
GUARD_HOME="$TMP_ROOT/destination-guard-home"
HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" install --destination "$GUARD_HOME" >/dev/null
DESTINATION_VICTIM="$TMP_ROOT/destination-victim"
mkdir -p "$DESTINATION_VICTIM"
printf 'do not touch me\n' > "$DESTINATION_VICTIM/marker.txt"

cyclops_update_help="$(HOME="$HOME_TEST" "$GUARD_HOME/bin/cyclops" update --help)"
[[ "$cyclops_update_help" == *'Usage: commPact-install'* ]] || fail "cyclops update --help did not show commPact-install usage"
cyclops_uninstall_help="$(HOME="$HOME_TEST" "$GUARD_HOME/bin/cyclops" uninstall --help)"
[[ "$cyclops_uninstall_help" == *'Usage: commPact-install'* ]] || fail "cyclops uninstall --help did not show commPact-install usage"

if HOME="$HOME_TEST" "$GUARD_HOME/bin/cyclops" update --destination "$DESTINATION_VICTIM" >/dev/null 2>&1; then
  fail "cyclops update accepted a caller-supplied --destination"
fi
assert_file "$DESTINATION_VICTIM/marker.txt"
[[ -d "$GUARD_HOME" ]] || fail "cyclops update guard removed its own install home"
if HOME="$HOME_TEST" "$GUARD_HOME/bin/cyclops" uninstall --restore "$DESTINATION_VICTIM" >/dev/null 2>&1; then
  fail "cyclops uninstall accepted a --restore option (advanced restore is commPact-install only now)"
fi
assert_file "$DESTINATION_VICTIM/marker.txt"
[[ -d "$GUARD_HOME" ]] || fail "cyclops uninstall guard removed its own install home"

printf 'commPact regression: cyclops start validates --session/--config values\n'
if COMMPACT_SOCKET="$SOCKET" "$DEST/bin/cyclops" start --session >/dev/null 2>&1; then
  fail "cyclops start accepted --session without a value"
fi
if COMMPACT_SOCKET="$SOCKET" "$DEST/bin/cyclops" start --config >/dev/null 2>&1; then
  fail "cyclops start accepted --config without a value"
fi
CONFLICT_CONFIG="$TMP_ROOT/cyclops-conflict.conf"
cat > "$CONFLICT_CONFIG" <<'EOF'
version=1
session=cyclops-conflict-saved-session
workdir=/tmp
layout=tiled
operator=operator
default_target=lead
agent_roles=lead
role=operator|sh
role=lead|sh
EOF
if COMMPACT_SOCKET="$SOCKET" "$DEST/bin/cyclops" start --config "$CONFLICT_CONFIG" --session some-other-session >/dev/null 2>&1; then
  fail "cyclops start silently ignored a --session that conflicts with the saved config"
fi

printf 'commPact regression: public installer canonicalizes a relative CYCLOPS_HOME\n'
RELATIVE_INSTALL_BASE="$TMP_ROOT/relative-install-base"
RELATIVE_INSTALL_BIN="$TMP_ROOT/relative-install-bin"
mkdir -p "$RELATIVE_INSTALL_BASE" "$RELATIVE_INSTALL_BIN"
(
  cd "$RELATIVE_INSTALL_BASE"
  env -u TMUX -u TMUX_PANE HOME="$HOME_TEST" CYCLOPS_SOURCE_DIR="$DEST" \
    CYCLOPS_HOME="relative-home" CYCLOPS_BIN_DIR="$RELATIVE_INSTALL_BIN" \
    sh "$RELEASE/frontend/static/install.sh" >/dev/null
)
RELATIVE_LINK_TARGET="$(readlink "$RELATIVE_INSTALL_BIN/cyclops")"
case "$RELATIVE_LINK_TARGET" in
  /*) : ;;
  *) fail "public installer linked cyclops to a relative target: $RELATIVE_LINK_TARGET" ;;
esac
[[ -x "$RELATIVE_INSTALL_BIN/cyclops" ]] || fail "public installer's relative-CYCLOPS_HOME symlink is dangling"
"$RELATIVE_INSTALL_BIN/cyclops" version >/dev/null || fail "public installer's relative-CYCLOPS_HOME symlink is broken"

printf 'commPact regression: one-command generic bootstrap\n'
QUICK_HOME="$TMP_ROOT/quick-home"
QUICK_PROJECT="$TMP_ROOT/quick-project"
mkdir -p "$QUICK_HOME" "$QUICK_PROJECT"
QUICK_PROJECT_REAL="$(cd "$QUICK_PROJECT" && pwd -P)"
(
  cd "$QUICK_PROJECT"
  env -u TMUX -u TMUX_PANE HOME="$QUICK_HOME" COMMPACT_SOCKET="$SOCKET" "$RELEASE/install.sh" \
    --session quickstart --roles driver,implementer --command sh >/dev/null
)
QUICK_CONFIG="$QUICK_HOME/.commPact/config/team.conf"
assert_file "$QUICK_CONFIG"
grep -q '^session=quickstart$' "$QUICK_CONFIG" || fail "quick setup session missing"
grep -q '^workdir='"$QUICK_PROJECT_REAL"'$' "$QUICK_CONFIG" || fail "quick setup workdir missing"
grep -q '^agent_roles=driver,implementer$' "$QUICK_CONFIG" || fail "quick setup roles missing"
quick_roles="$(tmux -S "$SOCKET" show-options -t quickstart -v @commPact_roles)"
assert_eq "$quick_roles" "operator,driver,implementer"
quick_default="$(tmux -S "$SOCKET" show-options -t quickstart -v @commPact_default_target)"
assert_eq "$quick_default" "driver"
if env -u TMUX -u TMUX_PANE HOME="$QUICK_HOME" COMMPACT_SOCKET="$SOCKET" "$QUICK_HOME/.commPact/bin/commPact-setup" \
  --session another --workdir "$QUICK_PROJECT" >/dev/null 2>&1; then
  fail "quick setup overwrote an existing configuration without --replace-config"
fi
ADOPT_CONFIG="$TMP_ROOT/adopt-generated.conf"
env -u TMUX -u TMUX_PANE COMMPACT_SOCKET="$SOCKET" "$DEST/bin/commPact-setup" \
  --config-only --config "$ADOPT_CONFIG" --session existing-team \
  --workdir "$QUICK_PROJECT" --operator facilitator --roles builder,checker --command sh >/dev/null
assert_file "$ADOPT_CONFIG"
grep -q '^operator=facilitator$' "$ADOPT_CONFIG" || fail "config-only setup operator missing"
grep -q '^agent_roles=builder,checker$' "$ADOPT_CONFIG" || fail "config-only setup roles missing"
if tmux -S "$SOCKET" has-session -t existing-team 2>/dev/null; then
  fail "config-only setup created a tmux session"
fi

printf 'commPact regression: message header and read guard\n'
sender_pane="$(tmux -S "$SOCKET" list-panes -s -t team-example -F '#{pane_id} #{@name}' | awk '$2 == "operator" {print $1; exit}')"
target_pane="$(tmux -S "$SOCKET" list-panes -s -t team-example -F '#{pane_id} #{@name}' | awk '$2 == "lead" {print $1; exit}')"
[[ -n "$sender_pane" && -n "$target_pane" && "$sender_pane" != "$target_pane" ]] || fail "message test panes missing"
sender_label="$(tmux -S "$SOCKET" display-message -t "$sender_pane" -p '#{@name}')"
session_win="$(tmux -S "$SOCKET" display-message -t "$sender_pane" -p '#{session_name}:#{window_index}.#{pane_index}')"
message_payload='header regression payload'
expected_header="[commPact from:${sender_label} pane:${sender_pane} at:${session_win}]"
COMMPACT_SOCKET="$SOCKET" "$DEST/bin/commPact" read "$target_pane" >/dev/null
TMUX_PANE="$sender_pane" COMMPACT_SOCKET="$SOCKET" "$DEST/bin/commPact" message "$target_pane" "$message_payload"
captured_message="$(tmux -S "$SOCKET" capture-pane -t "$target_pane" -p -J -S -20)"
[[ "$captured_message" == *"${expected_header} ${message_payload}"* ]] || fail "short message header or payload missing"
[[ "$captured_message" != *'load the commPact skill to reply'* ]] || fail "old message header text remains"
BLOCKED_CONFIG="$TMP_ROOT/blocked-generated.conf"
if TMUX_PANE="$target_pane" COMMPACT_SOCKET="$SOCKET" "$DEST/bin/commPact-setup" \
  --config "$BLOCKED_CONFIG" --session blocked-team --workdir "$QUICK_PROJECT" >/dev/null 2>&1; then
  fail "an agent pane created a new commPact session"
fi
[[ ! -e "$BLOCKED_CONFIG" ]] || fail "blocked setup wrote a configuration"

printf 'commPact regression: weighted rightmost split\n'
SPLIT_CONFIG="$TMP_ROOT/split.conf"
cat > "$SPLIT_CONFIG" <<EOF
version=1
session=split-example
workdir=/tmp
layout=columns
columns=3
split=operator:33,worker:66
operator=operator
default_target=lead
agent_roles=lead,worker,reviewer,observer
role=operator|sh
role=lead|sh
role=worker|sh
role=reviewer|sh
role=observer|sh
EOF
sed 's/layout=columns/layout=tiled/' "$SPLIT_CONFIG" > "$TMP_ROOT/split-tiled.conf"
if bash "$DEST/lib/commPact-config.sh" "$TMP_ROOT/split-tiled.conf" >/dev/null 2>&1; then
  fail "tiled config accepted split key"
fi
env -u TMUX -u TMUX_PANE COMMPACT_SOCKET="$SOCKET" "$DEST/bin/commPact-init" --config "$SPLIT_CONFIG" >/dev/null
operator_geom="$(tmux -S "$SOCKET" list-panes -s -t split-example -F '#{@name}|#{pane_left}|#{pane_width}|#{pane_top}|#{pane_height}' | awk -F'|' '$1 == "operator" {print; exit}')"
worker_geom="$(tmux -S "$SOCKET" list-panes -s -t split-example -F '#{@name}|#{pane_left}|#{pane_width}|#{pane_top}|#{pane_height}' | awk -F'|' '$1 == "worker" {print; exit}')"
lead_geom="$(tmux -S "$SOCKET" list-panes -s -t split-example -F '#{@name}|#{pane_left}|#{pane_width}|#{pane_top}|#{pane_height}' | awk -F'|' '$1 == "lead" {print; exit}')"
observer_geom="$(tmux -S "$SOCKET" list-panes -s -t split-example -F '#{@name}|#{pane_left}|#{pane_width}|#{pane_top}|#{pane_height}' | awk -F'|' '$1 == "observer" {print; exit}')"
IFS='|' read -r _ operator_x operator_w _ operator_h <<< "$operator_geom"
IFS='|' read -r _ worker_x worker_w _ worker_h <<< "$worker_geom"
IFS='|' read -r _ lead_x lead_w _ lead_h <<< "$lead_geom"
IFS='|' read -r _ observer_x observer_w _ observer_h <<< "$observer_geom"
[[ "$operator_x" == "$worker_x" && "$operator_w" == "$worker_w" ]] || fail "split pair is not one rightmost column"
((worker_h > operator_h)) || fail "weighted lower split is not taller"
[[ "$lead_x" == "$observer_x" && "$lead_w" == "$observer_w" ]] || fail "normal column width changed"
height_delta=$((lead_h - observer_h)); ((height_delta < 0)) && height_delta=$((-height_delta))
((height_delta <= 1)) || fail "normal column row heights differ by more than one"

printf 'commPact regression: manual watchdog\n'
STATE="$TMP_ROOT/state"
mkdir -p "$STATE"
touch "$STATE/last_update"
"$DEST/bin/commPact-state-watchdog" --state-dir "$STATE" >/dev/null

printf 'commPact regression: commPact-install validates --restore before deleting the install\n'
RESTORE_GUARD_HOME="$TMP_ROOT/restore-guard-home"
HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" install --destination "$RESTORE_GUARD_HOME" >/dev/null
RESTORE_GUARD_CHANGELOG_BEFORE="$(cat "$RESTORE_GUARD_HOME/CHANGELOG.md")"

if HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" uninstall --destination "$RESTORE_GUARD_HOME" \
  --restore "$TMP_ROOT/no-such-backup" >/dev/null 2>&1; then
  fail "commPact-install uninstall accepted a --restore path that doesn't exist"
fi
[[ -d "$RESTORE_GUARD_HOME" ]] || fail "an invalid --restore path deleted the install before validation"
[[ "$(cat "$RESTORE_GUARD_HOME/CHANGELOG.md")" == "$RESTORE_GUARD_CHANGELOG_BEFORE" ]] \
  || fail "an invalid --restore path left the install modified"
HOME="$HOME_TEST" "$RESTORE_GUARD_HOME/bin/cyclops" version >/dev/null \
  || fail "an invalid --restore path broke the launcher"

RESTORE_GUARD_NOT_A_DIR="$TMP_ROOT/restore-guard-not-a-dir"
printf 'not a directory\n' > "$RESTORE_GUARD_NOT_A_DIR"
if HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" uninstall --destination "$RESTORE_GUARD_HOME" \
  --restore "$RESTORE_GUARD_NOT_A_DIR" >/dev/null 2>&1; then
  fail "commPact-install uninstall accepted a --restore path that is a file, not a directory"
fi
[[ -d "$RESTORE_GUARD_HOME" ]] || fail "a file --restore path deleted the install before validation"
[[ "$(cat "$RESTORE_GUARD_HOME/CHANGELOG.md")" == "$RESTORE_GUARD_CHANGELOG_BEFORE" ]] \
  || fail "a file --restore path left the install modified"
HOME="$HOME_TEST" "$RESTORE_GUARD_HOME/bin/cyclops" version >/dev/null \
  || fail "a file --restore path broke the launcher"

printf 'commPact regression: commPact-install rejects overlapping restore paths\n'
RESTORE_GUARD_NESTED="$RESTORE_GUARD_HOME/nested-backup"
mkdir -p "$RESTORE_GUARD_NESTED"
printf 'nested backup marker\n' > "$RESTORE_GUARD_NESTED/marker.txt"
if HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" uninstall --destination "$RESTORE_GUARD_HOME" \
  --restore "$RESTORE_GUARD_NESTED" >/dev/null 2>&1; then
  fail "commPact-install accepted a restore path inside the install home"
fi
assert_file "$RESTORE_GUARD_NESTED/marker.txt"
[[ -x "$RESTORE_GUARD_HOME/bin/cyclops" ]] \
  || fail "a nested restore path deleted the install"

if HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" uninstall --destination "$RESTORE_GUARD_HOME" \
  --restore "$TMP_ROOT" >/dev/null 2>&1; then
  fail "commPact-install accepted a restore path containing the install home"
fi
assert_file "$RESTORE_GUARD_NESTED/marker.txt"
[[ -x "$RESTORE_GUARD_HOME/bin/cyclops" ]] \
  || fail "an ancestor restore path deleted the install"

if HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" uninstall --destination "$RESTORE_GUARD_HOME" \
  --restore "$RESTORE_GUARD_HOME/." >/dev/null 2>&1; then
  fail "commPact-install accepted a restore path resolving to the install home"
fi
assert_file "$RESTORE_GUARD_NESTED/marker.txt"
[[ -x "$RESTORE_GUARD_HOME/bin/cyclops" ]] \
  || fail "an equivalent restore path deleted the install"

printf 'commPact regression: explicit uninstall and restore\n'
HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" uninstall --destination "$DEST" --restore "$BACKUP" >/dev/null
assert_file "$DEST/old-marker"
[[ -d "$BACKUP" ]] || fail "explicit restore consumed backup"
HOME="$HOME_TEST" "$RELEASE/bin/commPact-install" uninstall --destination "$DEST" >/dev/null
[[ ! -e "$DEST" ]] || fail "uninstall left destination"
[[ -d "$BACKUP" ]] || fail "uninstall deleted backup"
shasum -a 256 "$HOME_TEST/.bashrc" "$HOME_TEST/.zshrc" "$HOME_TEST/.tmux.conf" > "$TMP_ROOT/config.final"
cmp "$TMP_ROOT/config.before" "$TMP_ROOT/config.final"

echo "commPact regression: PASS"
