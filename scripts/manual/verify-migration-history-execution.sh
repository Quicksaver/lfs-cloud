#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

cargo test --lib cli::tests::migrate_execution_uploads_every_historical_asset_version_before_reconfiguring -- --exact
cargo test --lib cli::tests::migrate_execution_does_not_reconfigure_after_a_partial_upload -- --exact

echo "Historical Git LFS migration execution and failure-safe reconfiguration verified"
