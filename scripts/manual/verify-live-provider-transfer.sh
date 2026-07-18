#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

is_enabled() {
  case "${1:-}" in
    1 | true | TRUE | yes | YES) return 0 ;;
    *) return 1 ;;
  esac
}

if ! is_enabled "${LFS_CLOUD_RUN_LIVE_TRANSFER_INTEGRATION:-}"; then
  echo "skipping live provider transfer check; set LFS_CLOUD_RUN_LIVE_TRANSFER_INTEGRATION=1, true, or yes to create disposable resources"
  exit 0
fi

if [[ -z "${LFS_CLOUD_GITHUB_TOKEN:-}" ]]; then
  echo "LFS_CLOUD_GITHUB_TOKEN is required" >&2
  echo "Use a token that can create and delete disposable repositories, and read collaborator permissions." >&2
  exit 1
fi

if [[ -z "${LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_JSON:-}" ]]; then
  if [[ -z "${LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_FILE:-}" ]]; then
    echo "LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_JSON or LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_FILE is required" >&2
    echo "Provide flat OAuth JSON with client_id, client_secret, refresh_token, and optional token_uri." >&2
    exit 1
  fi

  LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_JSON="$(<"${LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_FILE}")"
  export LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_JSON
fi

cargo test --test external_integrations live_server_upload_download_records_drive_and_sqlite_state -- --ignored --exact

echo "Live GitHub-authorized Google Drive transfer integration check passed"
