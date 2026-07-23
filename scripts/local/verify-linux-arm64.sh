#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/verify-linux-docker.sh
source "$SCRIPT_DIR/../lib/verify-linux-docker.sh"

if [[ "${1:-}" == "--help" ]] || [[ "${1:-}" == "-h" ]]; then
  cat <<'EOF'
Usage: ./scripts/local/verify-linux-arm64.sh

Run Linux ARM64 musl verification in the reusable image and container:
  image:     lfscloud-checks-linux-arm64:local
  container: lfscloud-checks-linux-arm64
EOF
  exit 0
fi
if (($# != 0)); then
  exit 2
fi

release_ui_initialize "[verify-linux-arm64]" "Verify Linux ARM64 release"
trap 'release_ui_finalize' EXIT

verify_linux_docker \
  "linux/arm64" \
  "aarch64-unknown-linux-musl" \
  "linux-arm64-musl" \
  "aarch64" \
  "$LOCAL_LINUX_ARM64_STATUS_CONTEXT" \
  "lfscloud-checks-linux-arm64:local" \
  "lfscloud-checks-linux-arm64" \
  "lfscloud-checks-linux-arm64-target"
