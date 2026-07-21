#!/usr/bin/env bash

ROTATION_INPUT=""
ROTATION_ENV_VALUE=""
ROTATION_REPO_ROOT=""
ROTATION_GITHUB_REPO=""
ROTATION_TEMP_FILE=""

rotation_finalize() {
  if [[ -n "$ROTATION_TEMP_FILE" ]] && [[ -f "$ROTATION_TEMP_FILE" ]]; then
    rm -f -- "$ROTATION_TEMP_FILE"
  fi

  ui_finalize
}

rotation_die() {
  ui_clear_live_state
  fail "$1"
  exit 1
}

rotation_resolve_repository() {
  local script_dir="$1"
  local remote_url
  local slug

  ROTATION_REPO_ROOT="$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null || true)"
  if [[ -z "$ROTATION_REPO_ROOT" ]]; then
    rotation_die "Could not resolve the repository root."
  fi

  remote_url="$(git -C "$ROTATION_REPO_ROOT" remote get-url origin 2>/dev/null || true)"
  case "$remote_url" in
    git@github.com:*)
      slug="${remote_url#git@github.com:}"
      ;;
    ssh://git@github.com/*)
      slug="${remote_url#ssh://git@github.com/}"
      ;;
    https://github.com/*)
      slug="${remote_url#https://github.com/}"
      ;;
    http://github.com/*)
      slug="${remote_url#http://github.com/}"
      ;;
    git://github.com/*)
      slug="${remote_url#git://github.com/}"
      ;;
    *)
      rotation_die "Could not resolve a GitHub owner/repository from the origin remote."
      ;;
  esac

  slug="${slug%.git}"
  if [[ ! "$slug" =~ ^[^/]+/[^/]+$ ]]; then
    rotation_die "Could not resolve a GitHub owner/repository from the origin remote."
  fi

  ROTATION_GITHUB_REPO="$slug"
}

rotation_require_local_env() {
  local env_file="$1"

  if [[ ! -f "$env_file" ]]; then
    rotation_die "Missing file: $env_file"
  fi

  if [[ -L "$env_file" ]]; then
    rotation_die "Refusing to update symbolic link: $env_file"
  fi

  if ! chmod 600 "$env_file"; then
    rotation_die "Could not restrict permissions on $env_file."
  fi
}

rotation_verify_github_cli() {
  ui_set_live_task_state "running" "Verify GitHub CLI authentication"

  if ! command -v gh >/dev/null 2>&1; then
    rotation_die "GitHub CLI is required. Install 'gh' and retry."
  fi

  if ! gh auth status --hostname github.com >/dev/null 2>&1; then
    rotation_die "GitHub CLI is not authenticated. Run 'gh auth login' and retry."
  fi

  ui_set_live_task_state "pass" "Verify GitHub CLI authentication"
  ui_clear_live_state
  pass "Verify GitHub CLI authentication"
}

rotation_prompt_secret() {
  local variable_name="$1"
  local input=""

  ui_clear_live_state
  printf '%s Set %s (leave empty to keep the current local value): ' "$UI_PREFIX" "$variable_name" > /dev/tty
  if ! IFS= read -r -s input < /dev/tty; then
    printf '\n' > /dev/tty
    rotation_die "Failed to read $variable_name input."
  fi
  printf '\n' > /dev/tty

  ROTATION_INPUT="$input"
  pass "Collect $variable_name"
}

rotation_validate_secret() {
  local variable_name="$1"
  local value="$2"

  if [[ -z "$value" ]]; then
    rotation_die "$variable_name must not be empty."
  fi

  if [[ "$value" =~ [[:space:]] ]]; then
    rotation_die "$variable_name must not contain whitespace."
  fi
}

rotation_read_env_value() {
  local env_file="$1"
  local variable_name="$2"
  local line
  local value=""
  local matches=0

  while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ "$line" == "$variable_name="* ]]; then
      value="${line#*=}"
      matches=$((matches + 1))
    fi
  done < "$env_file"

  if ((matches == 0)); then
    rotation_die "Could not find $variable_name in $env_file."
  fi

  if ((matches > 1)); then
    rotation_die "Found multiple $variable_name entries in $env_file."
  fi

  if [[ ${#value} -ge 2 ]]; then
    if [[ "$value" == \"*\" ]] || [[ "$value" == \'*\' ]]; then
      value="${value:1:${#value}-2}"
    fi
  fi

  rotation_validate_secret "$variable_name" "$value"
  ROTATION_ENV_VALUE="$value"
}

rotation_update_env_value() {
  local env_file="$1"
  local variable_name="$2"
  local value="$3"
  local line
  local matches=0

  ROTATION_TEMP_FILE="$(mktemp "${env_file}.tmp.XXXXXX")"
  chmod 600 "$ROTATION_TEMP_FILE"

  while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ "$line" == "$variable_name="* ]]; then
      printf '%s=%s\n' "$variable_name" "$value" >> "$ROTATION_TEMP_FILE"
      matches=$((matches + 1))
    else
      printf '%s\n' "$line" >> "$ROTATION_TEMP_FILE"
    fi
  done < "$env_file"

  if ((matches == 0)); then
    rotation_die "Could not find $variable_name in $env_file."
  fi

  if ((matches > 1)); then
    rotation_die "Found multiple $variable_name entries in $env_file."
  fi

  mv -f -- "$ROTATION_TEMP_FILE" "$env_file"
  ROTATION_TEMP_FILE=""
}

rotation_sync_github_secret() {
  local variable_name="$1"
  local value="$2"

  ui_set_live_task_state "running" "Sync $variable_name to GitHub secret"
  if ! printf '%s' "$value" | gh secret set "$variable_name" --repo "$ROTATION_GITHUB_REPO" >/dev/null 2>&1; then
    rotation_die "Failed to set GitHub secret $variable_name for $ROTATION_GITHUB_REPO."
  fi

  ui_set_live_task_state "pass" "Sync $variable_name to GitHub secret"
  ui_clear_live_state
  pass "Sync $variable_name to GitHub secret"
}
