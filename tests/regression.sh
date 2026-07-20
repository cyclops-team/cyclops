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
for command_name in commPact commPact-adopt commPact-init commPact-layout commPact-msg commPact-notice commPact-install commPact-setup commPact-state-watchdog; do
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
PATH="$DEST/bin:$PATH" HOME="$HOME_TEST" bash -c 'for name in commPact commPact-adopt commPact-init commPact-layout commPact-msg commPact-notice commPact-install commPact-setup commPact-state-watchdog; do command -v "$name" >/dev/null || exit 1; done; commPact-msg --help >/dev/null; commPact-setup --help >/dev/null; commPact-install version >/dev/null'
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
