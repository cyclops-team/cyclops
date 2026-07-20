#!/usr/bin/env bash
# Parse one flat commPact team configuration without executing its fields.

commPactConfigReset() {
  COMMPACT_CFG_PATH=""
  COMMPACT_CFG_VERSION=""
  COMMPACT_CFG_SESSION=""
  COMMPACT_CFG_WORKDIR=""
  COMMPACT_CFG_LAYOUT=""
  COMMPACT_CFG_COLUMNS=""
  COMMPACT_CFG_OPERATOR=""
  COMMPACT_CFG_DEFAULT_TARGET=""
  COMMPACT_CFG_AGENT_ROLES=""
  COMMPACT_CFG_SPLIT=""
  COMMPACT_CFG_SPLIT_ENABLED=0
  COMMPACT_CFG_SPLIT_LABELS=()
  COMMPACT_CFG_SPLIT_WEIGHTS=()
  COMMPACT_CFG_ROLE_COUNT=0
  COMMPACT_CFG_ROLE_LABELS=()
  COMMPACT_CFG_ROLE_COMMANDS=()
  COMMPACT_CFG_ROLE_LINES=()
  COMMPACT_CFG_FAILED=0
}

commPactConfigError() {
  local line="$1"
  shift
  echo "commPact-config: ${COMMPACT_CFG_PATH}:${line}: $*" >&2
  COMMPACT_CFG_FAILED=1
  return 1
}

commPactConfigLabelValid() {
  [[ "$1" =~ ^[a-z][a-z0-9_-]*$ ]]
}

commPactConfigHasRole() {
  local wanted="$1"
  local i
  for ((i = 0; i < COMMPACT_CFG_ROLE_COUNT; i++)); do
    [[ "${COMMPACT_CFG_ROLE_LABELS[i]}" == "$wanted" ]] && return 0
  done
  return 1
}

commPactConfigCsvValid() {
  local csv="$1"
  local item
  local old_ifs="$IFS"
  IFS=','
  for item in $csv; do
    [[ -n "$item" ]] || { IFS="$old_ifs"; return 1; }
    commPactConfigLabelValid "$item" || { IFS="$old_ifs"; return 1; }
  done
  IFS="$old_ifs"
  [[ -n "$csv" ]]
}

commPactConfigCsvContains() {
  local wanted="$1"
  local csv="$2"
  local item
  local old_ifs="$IFS"
  IFS=','
  for item in $csv; do
    [[ "$item" == "$wanted" ]] && { IFS="$old_ifs"; return 0; }
  done
  IFS="$old_ifs"
  return 1
}

commPactConfigLoad() {
  local path="$1"
  commPactConfigReset
  COMMPACT_CFG_PATH="$path"
  [[ -f "$path" ]] || { commPactConfigError 0 "config file not found"; return 1; }

  local line_no=0
  local line key value entry label command i
  local seen_version=0 seen_session=0 seen_workdir=0 seen_layout=0
  local seen_operator=0 seen_default=0 seen_agents=0 seen_columns=0 seen_split=0
  while IFS= read -r line || [[ -n "$line" ]]; do
    line_no=$((line_no + 1))
    line="${line%$'\r'}"
    [[ -z "$line" || "$line" == \#* ]] && continue

    if [[ "$line" == role=* ]]; then
      entry="${line#role=}"
      [[ "$entry" == *'|'* ]] || commPactConfigError "$line_no" "role must be LABEL|COMMAND"
      label="${entry%%|*}"
      command="${entry#*|}"
      commPactConfigLabelValid "$label" || commPactConfigError "$line_no" "invalid role label '$label'"
      [[ -n "$command" ]] || commPactConfigError "$line_no" "role command is empty"
      for ((i = 0; i < COMMPACT_CFG_ROLE_COUNT; i++)); do
        [[ "${COMMPACT_CFG_ROLE_LABELS[i]}" != "$label" ]] || commPactConfigError "$line_no" "duplicate role '$label'"
      done
      COMMPACT_CFG_ROLE_LABELS[COMMPACT_CFG_ROLE_COUNT]="$label"
      COMMPACT_CFG_ROLE_COMMANDS[COMMPACT_CFG_ROLE_COUNT]="$command"
      COMMPACT_CFG_ROLE_LINES[COMMPACT_CFG_ROLE_COUNT]="$line_no"
      COMMPACT_CFG_ROLE_COUNT=$((COMMPACT_CFG_ROLE_COUNT + 1))
      continue
    fi

    [[ "$line" == *=* ]] || commPactConfigError "$line_no" "expected key=value"
    key="${line%%=*}"
    value="${line#*=}"
    case "$key" in
      version)
        ((seen_version == 0)) || commPactConfigError "$line_no" "duplicate version"
        seen_version=1; COMMPACT_CFG_VERSION="$value" ;;
      session)
        ((seen_session == 0)) || commPactConfigError "$line_no" "duplicate session"
        seen_session=1; COMMPACT_CFG_SESSION="$value" ;;
      workdir)
        ((seen_workdir == 0)) || commPactConfigError "$line_no" "duplicate workdir"
        seen_workdir=1; COMMPACT_CFG_WORKDIR="$value" ;;
      layout)
        ((seen_layout == 0)) || commPactConfigError "$line_no" "duplicate layout"
        seen_layout=1; COMMPACT_CFG_LAYOUT="$value" ;;
      columns)
        ((seen_columns == 0)) || commPactConfigError "$line_no" "duplicate columns"
        seen_columns=1; COMMPACT_CFG_COLUMNS="$value" ;;
      operator)
        ((seen_operator == 0)) || commPactConfigError "$line_no" "duplicate operator"
        seen_operator=1; COMMPACT_CFG_OPERATOR="$value" ;;
      default_target)
        ((seen_default == 0)) || commPactConfigError "$line_no" "duplicate default_target"
        seen_default=1; COMMPACT_CFG_DEFAULT_TARGET="$value" ;;
      agent_roles)
        ((seen_agents == 0)) || commPactConfigError "$line_no" "duplicate agent_roles"
        seen_agents=1; COMMPACT_CFG_AGENT_ROLES="$value" ;;
      split)
        ((seen_split == 0)) || commPactConfigError "$line_no" "duplicate split"
        seen_split=1; COMMPACT_CFG_SPLIT="$value" ;;
      *) commPactConfigError "$line_no" "unknown key '$key'" ;;
    esac
  done < "$path"

  ((seen_version == 1)) || commPactConfigError 0 "missing version"
  [[ "$COMMPACT_CFG_VERSION" == "1" ]] || commPactConfigError 0 "version must be 1"
  ((seen_session == 1)) || commPactConfigError 0 "missing session"
  [[ -n "$COMMPACT_CFG_SESSION" ]] || commPactConfigError 0 "missing session"
  ((seen_workdir == 1)) || commPactConfigError 0 "missing workdir"
  [[ -n "$COMMPACT_CFG_WORKDIR" ]] || commPactConfigError 0 "missing workdir"
  ((seen_layout == 1)) || commPactConfigError 0 "missing layout"
  [[ "$COMMPACT_CFG_LAYOUT" == tiled || "$COMMPACT_CFG_LAYOUT" == columns ]] || commPactConfigError 0 "layout must be tiled or columns"
  if [[ "$COMMPACT_CFG_LAYOUT" == columns ]]; then
    ((seen_columns == 1)) || commPactConfigError 0 "columns is required for columns layout"
    [[ "$COMMPACT_CFG_COLUMNS" =~ ^[1-9][0-9]*$ ]] || commPactConfigError 0 "columns must be a positive integer"
  elif ((seen_columns == 1)); then
    [[ "$COMMPACT_CFG_COLUMNS" =~ ^[1-9][0-9]*$ ]] || commPactConfigError 0 "columns must be a positive integer when present"
  fi
  ((seen_operator == 1)) || commPactConfigError 0 "missing operator"
  ((seen_default == 1)) || commPactConfigError 0 "missing default_target"
  ((seen_agents == 1)) || commPactConfigError 0 "missing agent_roles"
  ((COMMPACT_CFG_ROLE_COUNT > 0)) || commPactConfigError 0 "at least one role is required"
  commPactConfigLabelValid "$COMMPACT_CFG_OPERATOR" || commPactConfigError 0 "invalid operator '$COMMPACT_CFG_OPERATOR'"
  commPactConfigLabelValid "$COMMPACT_CFG_DEFAULT_TARGET" || commPactConfigError 0 "invalid default_target '$COMMPACT_CFG_DEFAULT_TARGET'"
  commPactConfigCsvValid "$COMMPACT_CFG_AGENT_ROLES" || commPactConfigError 0 "invalid agent_roles"
  commPactConfigHasRole "$COMMPACT_CFG_OPERATOR" || commPactConfigError 0 "operator '$COMMPACT_CFG_OPERATOR' is not declared"
  commPactConfigHasRole "$COMMPACT_CFG_DEFAULT_TARGET" || commPactConfigError 0 "default_target '$COMMPACT_CFG_DEFAULT_TARGET' is not declared"
  commPactConfigCsvContains "$COMMPACT_CFG_OPERATOR" "$COMMPACT_CFG_AGENT_ROLES" && commPactConfigError 0 "operator must not be in agent_roles"
  local agent
  local old_ifs="$IFS"
  IFS=','
  for agent in $COMMPACT_CFG_AGENT_ROLES; do
    commPactConfigHasRole "$agent" || { IFS="$old_ifs"; commPactConfigError 0 "agent role '$agent' is not declared"; }
  done
  IFS="$old_ifs"
  if ((seen_split == 1)); then
    [[ "$COMMPACT_CFG_LAYOUT" == columns ]] || commPactConfigError 0 "split is valid only with columns layout"
    local split_first split_second split_label split_weight
    split_first="${COMMPACT_CFG_SPLIT%%,*}"
    split_second="${COMMPACT_CFG_SPLIT#*,}"
    [[ "$COMMPACT_CFG_SPLIT" == *,* && "$split_first" != "$COMMPACT_CFG_SPLIT" ]] || commPactConfigError 0 "split must contain two LABEL:WEIGHT entries"
    [[ "$split_second" != *,* && "$split_second" == *:* ]] || commPactConfigError 0 "split must contain exactly two LABEL:WEIGHT entries"
    split_label="${split_first%%:*}"
    split_weight="${split_first#*:}"
    commPactConfigLabelValid "$split_label" || commPactConfigError 0 "invalid split label '$split_label'"
    [[ "$split_weight" =~ ^[1-9][0-9]*$ ]] || commPactConfigError 0 "split weight must be a positive integer"
    COMMPACT_CFG_SPLIT_LABELS[0]="$split_label"
    COMMPACT_CFG_SPLIT_WEIGHTS[0]="$split_weight"
    split_label="${split_second%%:*}"
    split_weight="${split_second#*:}"
    commPactConfigLabelValid "$split_label" || commPactConfigError 0 "invalid split label '$split_label'"
    [[ "$split_weight" =~ ^[1-9][0-9]*$ ]] || commPactConfigError 0 "split weight must be a positive integer"
    [[ "${COMMPACT_CFG_SPLIT_LABELS[0]}" != "$split_label" ]] || commPactConfigError 0 "split labels must be distinct"
    COMMPACT_CFG_SPLIT_LABELS[1]="$split_label"
    COMMPACT_CFG_SPLIT_WEIGHTS[1]="$split_weight"
    commPactConfigHasRole "${COMMPACT_CFG_SPLIT_LABELS[0]}" || commPactConfigError 0 "split label '${COMMPACT_CFG_SPLIT_LABELS[0]}' is not declared"
    commPactConfigHasRole "${COMMPACT_CFG_SPLIT_LABELS[1]}" || commPactConfigError 0 "split label '${COMMPACT_CFG_SPLIT_LABELS[1]}' is not declared"
    ((COMMPACT_CFG_COLUMNS >= 2)) || commPactConfigError 0 "split requires at least two columns"
    ((COMMPACT_CFG_ROLE_COUNT - 2 >= COMMPACT_CFG_COLUMNS - 1)) || commPactConfigError 0 "split requires one remaining role per preceding column"
    COMMPACT_CFG_SPLIT_ENABLED=1
  fi
  if ((COMMPACT_CFG_ROLE_COUNT > 8)); then
    echo "commPact-config: warning: ${COMMPACT_CFG_ROLE_COUNT} roles configured; more than 8 may be hard to view" >&2
  fi
  ((COMMPACT_CFG_FAILED == 0)) || return 1
  return 0
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  [[ $# -eq 1 ]] || { echo "usage: commPact-config.sh PATH" >&2; exit 2; }
  commPactConfigLoad "$1" || exit 1
  echo "valid: $1 (${COMMPACT_CFG_ROLE_COUNT} roles)"
fi
