#!/usr/bin/env bash
# google-drive.sh — Rotate the Google Drive OAuth values used by live integration tests.
#
# Usage: ./scripts/keys/google-drive.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/terminal-ui.sh
source "$SCRIPT_DIR/../lib/terminal-ui.sh"
# shellcheck source=../lib/key-rotation.sh
source "$SCRIPT_DIR/../lib/key-rotation.sh"

ui_set_prefix "[google-drive]"
ui_set_render_mode "task_only"
ui_init
trap 'rotation_finalize' EXIT

ui_set_live_section_running "Rotate Google Drive integration credentials"

rotation_resolve_repository "$SCRIPT_DIR"
local_env_file="$ROTATION_REPO_ROOT/.env.local"
rotation_require_local_env "$local_env_file"
rotation_verify_github_cli

info "Open Google OAuth clients: https://console.cloud.google.com/auth/clients"

rotation_prompt_secret "LFS_CLOUD_GOOGLE_DRIVE_CLIENT_ID"
google_drive_client_id="$ROTATION_INPUT"
google_drive_client_id_is_new=false
if [[ -n "$google_drive_client_id" ]]; then
  google_drive_client_id_is_new=true
fi

rotation_prompt_secret "LFS_CLOUD_GOOGLE_DRIVE_CLIENT_SECRET"
google_drive_client_secret="$ROTATION_INPUT"
google_drive_client_secret_is_new=false
if [[ -n "$google_drive_client_secret" ]]; then
  google_drive_client_secret_is_new=true
fi

rotation_prompt_secret "LFS_CLOUD_GOOGLE_DRIVE_REFRESH_TOKEN"
google_drive_refresh_token="$ROTATION_INPUT"
google_drive_refresh_token_is_new=false
if [[ -n "$google_drive_refresh_token" ]]; then
  google_drive_refresh_token_is_new=true
fi

if [[ -z "$google_drive_client_id" ]]; then
  rotation_read_env_value "$local_env_file" "LFS_CLOUD_GOOGLE_DRIVE_CLIENT_ID"
  google_drive_client_id="$ROTATION_ENV_VALUE"
  skip "Update .env.local LFS_CLOUD_GOOGLE_DRIVE_CLIENT_ID"
else
  rotation_validate_secret "LFS_CLOUD_GOOGLE_DRIVE_CLIENT_ID" "$google_drive_client_id"
fi

if [[ -z "$google_drive_client_secret" ]]; then
  rotation_read_env_value "$local_env_file" "LFS_CLOUD_GOOGLE_DRIVE_CLIENT_SECRET"
  google_drive_client_secret="$ROTATION_ENV_VALUE"
  skip "Update .env.local LFS_CLOUD_GOOGLE_DRIVE_CLIENT_SECRET"
else
  rotation_validate_secret "LFS_CLOUD_GOOGLE_DRIVE_CLIENT_SECRET" "$google_drive_client_secret"
fi

if [[ -z "$google_drive_refresh_token" ]]; then
  rotation_read_env_value "$local_env_file" "LFS_CLOUD_GOOGLE_DRIVE_REFRESH_TOKEN"
  google_drive_refresh_token="$ROTATION_ENV_VALUE"
  skip "Update .env.local LFS_CLOUD_GOOGLE_DRIVE_REFRESH_TOKEN"
else
  rotation_validate_secret "LFS_CLOUD_GOOGLE_DRIVE_REFRESH_TOKEN" "$google_drive_refresh_token"
fi

# Update local values only after every prompt and retained value has validated.
if [[ "$google_drive_client_id_is_new" == true ]]; then
  rotation_update_env_value "$local_env_file" "LFS_CLOUD_GOOGLE_DRIVE_CLIENT_ID" "$google_drive_client_id"
  pass "Update .env.local LFS_CLOUD_GOOGLE_DRIVE_CLIENT_ID"
fi

if [[ "$google_drive_client_secret_is_new" == true ]]; then
  rotation_update_env_value "$local_env_file" "LFS_CLOUD_GOOGLE_DRIVE_CLIENT_SECRET" "$google_drive_client_secret"
  pass "Update .env.local LFS_CLOUD_GOOGLE_DRIVE_CLIENT_SECRET"
fi

if [[ "$google_drive_refresh_token_is_new" == true ]]; then
  rotation_update_env_value "$local_env_file" "LFS_CLOUD_GOOGLE_DRIVE_REFRESH_TOKEN" "$google_drive_refresh_token"
  pass "Update .env.local LFS_CLOUD_GOOGLE_DRIVE_REFRESH_TOKEN"
fi

rotation_sync_github_secret "LFS_CLOUD_GOOGLE_DRIVE_CLIENT_ID" "$google_drive_client_id"
rotation_sync_github_secret "LFS_CLOUD_GOOGLE_DRIVE_CLIENT_SECRET" "$google_drive_client_secret"
rotation_sync_github_secret "LFS_CLOUD_GOOGLE_DRIVE_REFRESH_TOKEN" "$google_drive_refresh_token"
