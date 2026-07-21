#!/usr/bin/env bash
# google-drive.sh — Select the gcloud ADC file used by live integration tests.
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

ui_set_live_section_running "Configure Google Drive integration credentials"

rotation_resolve_repository "$SCRIPT_DIR"
local_env_file="$ROTATION_REPO_ROOT/.env.local"
rotation_require_local_env "$local_env_file"
rotation_verify_github_cli

if ! command -v gcloud >/dev/null 2>&1; then
  rotation_die "Google Cloud CLI is required. Install 'gcloud' and retry."
fi

ui_clear_live_state
printf '%s Set LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR to the isolated gcloud config directory: ' "$UI_PREFIX" > /dev/tty
if ! IFS= read -r google_drive_config_dir < /dev/tty; then
  printf '\n' > /dev/tty
  rotation_die "Failed to read the Google Drive gcloud config directory."
fi

if [[ -z "$google_drive_config_dir" ]]; then
  rotation_read_env_value "$local_env_file" "LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR"
  google_drive_config_dir="$ROTATION_ENV_VALUE"
  skip "Update .env.local LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR"
fi

if [[ ! -d "$google_drive_config_dir" ]]; then
  rotation_die "LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR must point to a readable directory."
fi
google_drive_config_dir="$(cd "$google_drive_config_dir" && pwd -P)"
google_drive_credentials_file="$google_drive_config_dir/application_default_credentials.json"
if [[ ! -r "$google_drive_credentials_file" ]]; then
  rotation_die "The configured directory does not contain a readable application_default_credentials.json."
fi

if grep -q '^LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR=' "$local_env_file"; then
  rotation_update_env_value "$local_env_file" "LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR" "$google_drive_config_dir"
else
  printf '%s=%s\n' "LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR" "$google_drive_config_dir" >> "$local_env_file"
fi
pass "Update .env.local LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR"

google_drive_adc_json="$(<"$google_drive_credentials_file")"
rotation_sync_github_secret "LFS_CLOUD_GOOGLE_DRIVE_ADC_JSON" "$google_drive_adc_json"
