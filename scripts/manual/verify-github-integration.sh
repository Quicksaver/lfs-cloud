#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

is_enabled() {
  case "${1:-}" in
    1 | true | TRUE | yes | YES) return 0 ;;
    *) return 1 ;;
  esac
}

if ! is_enabled "${LFS_CLOUD_RUN_GITHUB_INTEGRATION:-}"; then
  echo "skipping GitHub integration check; set LFS_CLOUD_RUN_GITHUB_INTEGRATION=1, true, or yes to create a disposable repo"
  exit 0
fi

if [[ -z "${LFS_CLOUD_GITHUB_TOKEN:-}" ]]; then
  echo "LFS_CLOUD_GITHUB_TOKEN is required" >&2
  echo "Use a token that can create and delete disposable repositories, and read collaborator permissions." >&2
  exit 1
fi

cargo test --test external_integrations github_disposable_repo_permission_check -- --ignored --exact

echo "GitHub disposable repository integration check passed"
