#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

cd "$REPO_ROOT"

cargo test --lib upload_migration_objects_skips_existing_and_uploads_verified_sources
cargo test --lib upload_migration_objects_rechecks_source_bytes_before_upload
cargo test --lib upload_migration_objects_rejects_returned_object_mismatch
cargo test --lib upload_migration_objects_rejects_provider_id_mismatch
cargo test --lib upload_migration_objects_rejects_empty_backend_id
cargo test --lib migration_upload_source_prefers_git_lfs_media_over_shared_cache

echo "migration upload idempotence and source-byte verification tests passed"
