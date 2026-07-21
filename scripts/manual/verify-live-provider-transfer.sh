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

if [[ -z "${LFS_CLOUD_GITHUB_PAT:-}" ]]; then
  echo "LFS_CLOUD_GITHUB_PAT is required" >&2
  exit 1
fi
echo "The PAT must grant repository creation and deletion for disposable smoke resources." >&2

if [[ -z "${LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR:-}" ]]; then
  echo "LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR is required" >&2
  exit 1
fi
if [[ ! -d "$LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR" ]] || [[ ! -r "$LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR/application_default_credentials.json" ]]; then
  echo "LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR must point to a readable directory containing application_default_credentials.json" >&2
  exit 1
fi
if ! command -v gcloud >/dev/null 2>&1; then
  echo "gcloud is required for the live provider transfer check" >&2
  exit 1
fi

cargo test --test external_integrations black_box_git_lfs_push_fetch_uses_live_github_and_drive -- --ignored --exact

echo "Black-box Git LFS transfer through the compiled LFS Cloud server passed"
