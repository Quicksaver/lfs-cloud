#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

is_enabled() {
  case "${1:-}" in
    1 | true | TRUE | yes | YES) return 0 ;;
    *) return 1 ;;
  esac
}

if ! is_enabled "${LFS_CLOUD_RUN_GOOGLE_DRIVE_INTEGRATION:-}"; then
  echo "skipping Google Drive integration check; set LFS_CLOUD_RUN_GOOGLE_DRIVE_INTEGRATION=1, true, or yes to create a disposable folder"
  exit 0
fi

for credential_env in \
  LFS_CLOUD_GOOGLE_DRIVE_CLIENT_ID \
  LFS_CLOUD_GOOGLE_DRIVE_CLIENT_SECRET \
  LFS_CLOUD_GOOGLE_DRIVE_REFRESH_TOKEN; do
  if [[ -z "${!credential_env:-}" ]]; then
    echo "$credential_env is required" >&2
    exit 1
  fi
done

cargo test --test external_integrations google_drive_disposable_folder_root_validation -- --ignored --exact

echo "Google Drive disposable folder integration check passed"
