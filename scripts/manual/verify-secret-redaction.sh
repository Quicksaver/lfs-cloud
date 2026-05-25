#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

cargo test --all-targets server_config_debug_redacts_github_oauth_client_secret
cargo test --all-targets redacts
cargo test --all-targets leaking

echo "secret redaction checks passed"
