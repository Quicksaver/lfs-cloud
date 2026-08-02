#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

cargo test --lib github_auth::tests::personal_access_token_login_issues_local_session_for_presented_github_user -- --exact
cargo test --lib server::tests::batch_route_authorizes_download_as_read_and_upload_as_write -- --exact
cargo test --lib server::tests::batch_route_rejects_repository_permission_denials -- --exact
cargo test --lib server::tests::default_batch_authorizer_checks_github_permissions -- --exact

echo "Per-user GitHub identity and repository read/write authorization verified"
