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

for credential_env in \
  LFS_CLOUD_GOOGLE_DRIVE_CLIENT_ID \
  LFS_CLOUD_GOOGLE_DRIVE_CLIENT_SECRET \
  LFS_CLOUD_GOOGLE_DRIVE_REFRESH_TOKEN; do
  if [[ -z "${!credential_env:-}" ]]; then
    echo "$credential_env is required" >&2
    exit 1
  fi
done

cargo test --test external_integrations black_box_git_lfs_push_fetch_uses_live_github_and_drive -- --ignored --exact

echo "Black-box Git LFS transfer through the compiled LFS Cloud server passed"
