#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

if [[ "${LFS_CLOUD_RUN_GOOGLE_DRIVE_INTEGRATION:-}" != "1" ]]; then
  echo "skipping Google Drive integration check; set LFS_CLOUD_RUN_GOOGLE_DRIVE_INTEGRATION=1 to create a disposable folder"
  exit 0
fi

if [[ -z "${LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_JSON:-}" && -z "${LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_FILE:-}" ]]; then
  echo "LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_JSON or LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_FILE is required" >&2
  echo "Provide flat OAuth JSON with client_id, client_secret, refresh_token, and optional token_uri." >&2
  exit 1
fi

cargo test --test external_integrations google_drive_disposable_folder_root_validation -- --ignored --exact

echo "Google Drive disposable folder integration check passed"
