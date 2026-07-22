#!/usr/bin/env bash
# slack.sh — Rotate the Slack webhook used for workflow failure notifications.
#
# Usage: ./scripts/keys/slack.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/terminal-ui.sh
source "$SCRIPT_DIR/../lib/terminal-ui.sh"
# shellcheck source=../lib/key-rotation.sh
source "$SCRIPT_DIR/../lib/key-rotation.sh"

ui_set_prefix "[slack]"
ui_set_render_mode "task_only"
ui_init
trap 'rotation_finalize' EXIT

ui_set_live_section_running "Rotate Slack workflow notification webhook"

rotation_resolve_repository "$SCRIPT_DIR"
local_env_file="$ROTATION_REPO_ROOT/.env.local"
rotation_require_local_env "$local_env_file"
rotation_verify_github_cli

info "Create an incoming webhook for Slack channel C0BJY1M2WR4."
info "Slack webhook settings: https://api.slack.com/apps"

rotation_validate_slack_webhook_url() {
  local value="$1"

  rotation_validate_secret "SLACK_WEBHOOK_URL" "$value"
  if [[ ! "$value" =~ ^https://hooks\.slack\.com/services/[^/?#[:space:]]+/[^/?#[:space:]]+/[^/?#[:space:]]+$ ]]; then
    rotation_die "SLACK_WEBHOOK_URL must be a channel-bound https://hooks.slack.com/services/... URL."
  fi
}

rotation_prompt_secret "SLACK_WEBHOOK_URL"
slack_webhook_url="$ROTATION_INPUT"

if [[ -z "$slack_webhook_url" ]]; then
  rotation_read_env_value "$local_env_file" "SLACK_WEBHOOK_URL"
  slack_webhook_url="$ROTATION_ENV_VALUE"
  rotation_validate_slack_webhook_url "$slack_webhook_url"
  skip "Update .env.local SLACK_WEBHOOK_URL"
else
  rotation_validate_slack_webhook_url "$slack_webhook_url"
  if grep -q '^SLACK_WEBHOOK_URL=' "$local_env_file"; then
    rotation_update_env_value "$local_env_file" "SLACK_WEBHOOK_URL" "$slack_webhook_url"
  else
    printf '%s=%s\n' "SLACK_WEBHOOK_URL" "$slack_webhook_url" >> "$local_env_file"
  fi
  pass "Update .env.local SLACK_WEBHOOK_URL"
fi

rotation_sync_github_secret "SLACK_WEBHOOK_URL" "$slack_webhook_url"
