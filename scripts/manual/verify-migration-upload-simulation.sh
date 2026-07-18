#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

cd "$REPO_ROOT"

# These focused tests exercise the migration upload contract through an
# in-process fake storage provider. They do not contact Google Drive.
cargo test --lib upload_migration_objects_skips_existing_and_uploads_verified_sources
cargo test --lib upload_migration_objects_rechecks_source_bytes_before_upload
cargo test --lib upload_migration_objects_rejects_returned_object_mismatch
cargo test --lib upload_migration_objects_rejects_provider_id_mismatch
cargo test --lib upload_migration_objects_rejects_empty_backend_id
cargo test --lib migration_upload_source_prefers_git_lfs_media_over_shared_cache

echo "simulated migration upload contract tests passed"
