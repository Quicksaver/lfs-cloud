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

if [[ -z "${LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR:-}" ]]; then
  echo "LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR is required" >&2
  exit 1
fi
if [[ ! -d "$LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR" ]] || [[ ! -r "$LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR/application_default_credentials.json" ]]; then
  echo "LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR must point to a readable directory containing application_default_credentials.json" >&2
  exit 1
fi
if ! command -v gcloud >/dev/null 2>&1; then
  echo "gcloud is required for the Google Drive integration check" >&2
  exit 1
fi

cargo test --test external_integrations google_drive_disposable_folder_root_validation -- --ignored --exact

echo "Google Drive disposable folder integration check passed"
