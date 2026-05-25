#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

cd "$REPO_ROOT"

if ! git lfs version >/dev/null 2>&1; then
  echo "git lfs is required for migration source-fetch verification" >&2
  exit 1
fi

cargo test source_fetch_downloads_missing_objects_without_changing_worktree_files -- --ignored --nocapture
