#!/usr/bin/env bash
# github.sh — Rotate the GitHub token used by live integration tests.
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

ui_set_live_section_running "Rotate GitHub integration token"

rotation_resolve_repository "$SCRIPT_DIR"
local_env_file="$ROTATION_REPO_ROOT/.env.local"
rotation_require_local_env "$local_env_file"
rotation_verify_github_cli

info "Use a token that can create and delete disposable repositories and read collaborator permissions."
info "GitHub token settings: https://github.com/settings/tokens"

rotation_prompt_secret "LFS_CLOUD_GITHUB_TOKEN"
github_token="$ROTATION_INPUT"

if [[ -z "$github_token" ]]; then
  rotation_read_env_value "$local_env_file" "LFS_CLOUD_GITHUB_TOKEN"
  github_token="$ROTATION_ENV_VALUE"
  skip "Update .env.local LFS_CLOUD_GITHUB_TOKEN"
else
  rotation_validate_secret "LFS_CLOUD_GITHUB_TOKEN" "$github_token"
  rotation_update_env_value "$local_env_file" "LFS_CLOUD_GITHUB_TOKEN" "$github_token"
  pass "Update .env.local LFS_CLOUD_GITHUB_TOKEN"
fi

rotation_sync_github_secret "LFS_CLOUD_GITHUB_TOKEN" "$github_token"
