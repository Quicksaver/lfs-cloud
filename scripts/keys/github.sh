#!/usr/bin/env bash
# github.sh — Rotate the GitHub PAT used by live integration tests.
#
# Usage: ./scripts/keys/github.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/terminal-ui.sh
source "$SCRIPT_DIR/../lib/terminal-ui.sh"
# shellcheck source=../lib/key-rotation.sh
source "$SCRIPT_DIR/../lib/key-rotation.sh"

ui_set_prefix "[github]"
ui_set_render_mode "task_only"
ui_init
trap 'rotation_finalize' EXIT

ui_set_live_section_running "Rotate GitHub smoke credentials"

rotation_resolve_repository "$SCRIPT_DIR"
local_env_file="$ROTATION_REPO_ROOT/.env.local"
rotation_require_local_env "$local_env_file"
rotation_verify_github_cli

info "Use a classic PAT with repo and delete_repo for disposable smoke resources."
info "GitHub token settings: https://github.com/settings/tokens"

collect_github_credential() {
  local variable_name="$1"
  local value

  rotation_prompt_secret "$variable_name"
  value="$ROTATION_INPUT"

  if [[ -z "$value" ]]; then
    rotation_read_env_value "$local_env_file" "$variable_name"
    value="$ROTATION_ENV_VALUE"
    skip "Update .env.local $variable_name"
  else
    rotation_validate_secret "$variable_name" "$value"
    if grep -q "^${variable_name}=" "$local_env_file"; then
      rotation_update_env_value "$local_env_file" "$variable_name" "$value"
    else
      printf '%s=%s\n' "$variable_name" "$value" >> "$local_env_file"
    fi
    pass "Update .env.local $variable_name"
  fi

  ROTATION_GITHUB_CREDENTIAL="$value"
}

collect_github_credential "LFS_CLOUD_GITHUB_PAT"
github_pat="$ROTATION_GITHUB_CREDENTIAL"

rotation_sync_github_secret "LFS_CLOUD_GITHUB_PAT" "$github_pat"
