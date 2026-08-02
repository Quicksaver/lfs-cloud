#!/usr/bin/env bash
set -euo pipefail

# Hermetic unit-test verifier: no provider credentials or environment flags
# are required, and the repository root is selected relative to this script.
cd "$(dirname "$0")/../.."

cargo test --lib cli::migration::tests::migration_reconciles_and_uploads_only_server_missing_objects -- --exact
cargo test --lib git::tests::worktree_config_preserves_target_and_remote_legacy_lfs_urls -- --exact
cargo test --lib migration::discovery_tests::committed_remote_endpoint_remains_the_source_after_lfscloud_configuration -- --exact
cargo test --lib migration::fetch_tests::source_fetch_command_can_override_committed_lfscloud_target_with_legacy_url -- --exact

echo "Server-mediated migration reconciliation and follow-up source configuration verified"
