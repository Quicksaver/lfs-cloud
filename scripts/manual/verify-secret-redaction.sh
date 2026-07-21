#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

redaction_tests=(
  "cli::tests::init_summary_redacts_sensitive_previous_lfs_url"
  "cli::tests::status_redacts_unsafe_server_override_before_route_validation"
  "cli::tests::status_reports_failures_without_leaking_credential_secrets"
  "credentials::tests::command_stderr_redacts_token_before_truncating"
  "credentials::tests::lookup_rejects_invalid_local_lfs_tokens_without_leaking_them"
  "credentials::tests::lookup_rejects_non_utf8_stdout_without_leaking_output"
  "git::tests::debug_redacts_credentialed_url_defensively"
  "github_auth::tests::callback_debug_output_redacts_code_and_state"
  "github_auth::tests::callback_route_state_debug_redacts_provider_secret_and_pending_states"
  "github_auth::tests::debug_output_redacts_csrf_state"
  "github_auth::tests::token_debug_output_redacts_access_token"
  "github_auth::tests::user_client_redacts_long_token_before_truncating_upstream_error_message"
  "github_auth::tests::user_client_redacts_token_from_upstream_error_message"
  "google_drive::tests::refresher_redacts_credentials_from_unsupported_token_type"
  "google_drive::tests::refresher_redacts_credentials_from_upstream_diagnostics"
  "server_config::tests::raw_repository_provider_debug_redacts_github_oauth_client_secret"
  "server_config::tests::server_config_debug_redacts_github_oauth_client_secret"
  "server::tests::server_tracing_events_never_render_request_or_provider_secrets"
  "sessions::tests::session_token_validates_restored_secret_and_redacts_debug"
)

# These regressions use Unix shell fixtures and are not compiled into Windows
# test binaries. Keep requiring them on every target where cfg(unix) includes them.
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) ;;
  *)
    redaction_tests+=(
      "credentials::tests::lookup_failure_suppresses_helper_stderr"
      "credentials::tests::approve_failure_redacts_token_from_command_error"
    )
    ;;
esac

available_tests="$(cargo test --all-targets -- --list)"

for test_name in "${redaction_tests[@]}"; do
  if ! grep -Fqx "$test_name: test" <<<"$available_tests"; then
    echo "expected redaction test not found: $test_name" >&2
    exit 1
  fi

  cargo test --all-targets "$test_name" -- --exact
done

echo "secret redaction checks passed"
