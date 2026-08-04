#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

case "${LFS_CLOUD_RUN_NATIVE_KEYRING_SMOKE:-}" in
  1 | true | TRUE | yes | YES) ;;
  *)
    echo "set LFS_CLOUD_RUN_NATIVE_KEYRING_SMOKE=1 to create and remove a disposable native credential" >&2
    exit 1
    ;;
esac

cargo test --lib \
  session_keys::tests::native_credential_store_generates_reloads_rotates_and_cleans_up \
  -- --ignored --exact

echo "Native credential-store session key generation, reload, rotation, and cleanup passed"
