#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/verify-linux-docker.sh
source "$SCRIPT_DIR/../lib/verify-linux-docker.sh"

if [[ "${1:-}" == "--help" ]] || [[ "${1:-}" == "-h" ]]; then
  cat <<'EOF'
Usage: ./scripts/local/verify-linux-x86-64.sh

Run Linux x86-64 musl verification in the reusable image and container:
  image:     lfscloud-checks-linux-x86-64:local
  container: lfscloud-checks-linux-x86-64
EOF
  exit 0
fi
if (($# != 0)); then
  exit 2
fi

release_ui_initialize "[verify-linux-x86-64]" "Verify Linux x86-64 release"
trap 'release_ui_finalize' EXIT

verify_linux_docker \
  "linux/amd64" \
  "x86_64-unknown-linux-musl" \
  "linux-x86_64-musl" \
  "x86_64" \
  "$LOCAL_LINUX_X86_64_STATUS_CONTEXT" \
  "lfscloud-checks-linux-x86-64:local" \
  "lfscloud-checks-linux-x86-64" \
  "lfscloud-checks-linux-x86-64-target"
