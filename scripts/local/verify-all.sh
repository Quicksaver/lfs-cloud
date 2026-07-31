#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/release-common.sh
source "$SCRIPT_DIR/../lib/release-common.sh"

VERIFY_ALL_LABELS=()
VERIFY_ALL_COMMANDS=()
VERIFY_ALL_PIDS=()
VERIFY_ALL_LOG_FILES=()
VERIFY_ALL_STATUS_FILES=()
VERIFY_ALL_LOG_OFFSETS=()
VERIFY_ALL_EXIT_CODES=()
VERIFY_ALL_STATES=()
VERIFY_ALL_TEMP_DIR=""

verify_all_usage() {
  cat <<'EOF'
Usage: ./scripts/local/verify-all.sh

Run every deterministic local verifier supported by the current system in
parallel. macOS runs the native macOS verifier, Windows runs the native Windows
verifier, and a responsive Docker Linux engine runs both Linux verifiers. Each
verifier records its own local-checks/* commit status and release artifact.
EOF
}

verify_all_docker_is_runnable() {
  command -v docker >/dev/null 2>&1 \
    && [[ "$(docker info --format '{{.OSType}}' 2>/dev/null)" == "linux" ]]
}

verify_all_configure_default_commands() {
  local system_name

  VERIFY_ALL_LABELS=()
  VERIFY_ALL_COMMANDS=()
  system_name="$(uname -s)"

  case "$system_name" in
    Darwin)
      VERIFY_ALL_LABELS+=("macOS ARM64")
      VERIFY_ALL_COMMANDS+=("$SCRIPT_DIR/verify-macos.sh")
      ;;
    CYGWIN* | MINGW* | MSYS* | Windows_NT)
      VERIFY_ALL_LABELS+=("Windows x86-64")
      VERIFY_ALL_COMMANDS+=("$SCRIPT_DIR/verify-windows.ps1")
      ;;
  esac

  if verify_all_docker_is_runnable; then
    VERIFY_ALL_LABELS+=("Linux ARM64" "Linux x86-64")
    VERIFY_ALL_COMMANDS+=(
      "$SCRIPT_DIR/verify-linux-arm64.sh"
      "$SCRIPT_DIR/verify-linux-x86-64.sh"
    )
  fi
}

verify_all_validate_command() {
  local command_path="$1"

  if [[ "$command_path" == *.ps1 ]]; then
    [[ -f "$command_path" ]] \
      || release_die "Verifier does not exist: $command_path"
    command -v pwsh >/dev/null 2>&1 \
      || release_die "PowerShell 7 is required to run verifier: $command_path"
  elif [[ ! -x "$command_path" ]]; then
    release_die "Verifier is not executable: $command_path"
  fi
}

verify_all_run_command() {
  local command_path="$1"

  if [[ "$command_path" == *.ps1 ]]; then
    pwsh -NoProfile -File "$command_path"
  else
    "$command_path"
  fi
}

verify_all_child_pids() {
  local parent_pid="$1"

  ps -axo pid=,ppid= \
    | awk -v parent_pid="$parent_pid" '$2 == parent_pid { print $1 }'
}

verify_all_signal_tree() {
  local signal="$1"
  local pid="$2"
  local child_pid

  while IFS= read -r child_pid; do
    [[ -n "$child_pid" ]] || continue
    verify_all_signal_tree "$signal" "$child_pid"
  done < <(verify_all_child_pids "$pid")

  kill "-$signal" "$pid" 2>/dev/null || true
}

verify_all_cleanup() {
  local active=0
  local idx
  local pid

  for ((idx = 0; idx < ${#VERIFY_ALL_PIDS[@]}; idx++)); do
    if [[ "${VERIFY_ALL_STATES[$idx]:-}" != "done" ]]; then
      pid="${VERIFY_ALL_PIDS[$idx]:-}"
      if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        active=1
        verify_all_signal_tree TERM "$pid"
      fi
    fi
  done

  if ((active == 1)); then
    sleep 0.2
    for ((idx = 0; idx < ${#VERIFY_ALL_PIDS[@]}; idx++)); do
      if [[ "${VERIFY_ALL_STATES[$idx]:-}" != "done" ]]; then
        pid="${VERIFY_ALL_PIDS[$idx]:-}"
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
          verify_all_signal_tree KILL "$pid"
        fi
      fi
    done
  fi

  for pid in "${VERIFY_ALL_PIDS[@]:-}"; do
    [[ -n "$pid" ]] || continue
    wait "$pid" 2>/dev/null || true
  done

  if [[ -n "$VERIFY_ALL_TEMP_DIR" ]]; then
    rm -rf -- "$VERIFY_ALL_TEMP_DIR"
    VERIFY_ALL_TEMP_DIR=""
  fi
}

verify_all_drain_log() {
  local idx="$1"
  local log_file="${VERIFY_ALL_LOG_FILES[$idx]}"
  local previous_line="${VERIFY_ALL_LOG_OFFSETS[$idx]:-0}"
  local total_lines
  local first_line
  local line

  total_lines="$(wc -l < "$log_file" | tr -d '[:space:]')"
  if ((total_lines <= previous_line)); then
    return
  fi

  first_line=$((previous_line + 1))
  while IFS= read -r line || [[ -n "$line" ]]; do
    ui_append_rolling_slot_output "$idx" "$line"
  done < <(sed -n "${first_line},${total_lines}p" "$log_file")
  VERIFY_ALL_LOG_OFFSETS[$idx]="$total_lines"
}

verify_all_print_failed_logs() {
  local idx
  local output

  [[ "$LIVE_REGION_ENABLED" == true ]] || return

  for ((idx = 0; idx < ${#VERIFY_ALL_LABELS[@]}; idx++)); do
    if [[ "${VERIFY_ALL_EXIT_CODES[$idx]:-1}" != "0" ]]; then
      release_warn "${VERIFY_ALL_LABELS[$idx]} verifier output"
      output="$(cat "${VERIFY_ALL_LOG_FILES[$idx]}")"
      ui_log_persistent_raw_batch "$output" "$YELLOW"
    fi
  done
}

verify_all_run_parallel() {
  local count="${#VERIFY_ALL_COMMANDS[@]}"
  local completed=0
  local failed=0
  local idx
  local command_path
  local exit_code
  local status_file
  local status_tmp

  if ((count == 0)) || ((count != ${#VERIFY_ALL_LABELS[@]})); then
    release_die "Parallel verification requires matching command and label lists."
  fi

  VERIFY_ALL_TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/lfscloud-verify-all.XXXXXX")"
  VERIFY_ALL_PIDS=()
  VERIFY_ALL_LOG_FILES=()
  VERIFY_ALL_STATUS_FILES=()
  VERIFY_ALL_LOG_OFFSETS=()
  VERIFY_ALL_EXIT_CODES=()
  VERIFY_ALL_STATES=()

  ui_enable_rolling_slots "$count" 3
  for ((idx = 0; idx < count; idx++)); do
    command_path="${VERIFY_ALL_COMMANDS[$idx]}"
    verify_all_validate_command "$command_path"

    VERIFY_ALL_LOG_FILES[$idx]="$VERIFY_ALL_TEMP_DIR/verifier-$idx.log"
    VERIFY_ALL_STATUS_FILES[$idx]="$VERIFY_ALL_TEMP_DIR/verifier-$idx.status"
    VERIFY_ALL_LOG_OFFSETS[$idx]=0
    VERIFY_ALL_EXIT_CODES[$idx]=""
    VERIFY_ALL_STATES[$idx]="running"
    : > "${VERIFY_ALL_LOG_FILES[$idx]}"
    ui_set_slot "$idx" "running" "${VERIFY_ALL_LABELS[$idx]}"
  done

  for ((idx = 0; idx < count; idx++)); do
    command_path="${VERIFY_ALL_COMMANDS[$idx]}"
    status_file="${VERIFY_ALL_STATUS_FILES[$idx]}"
    status_tmp="$status_file.tmp"
    (
      set +e
      verify_all_run_command "$command_path" > "${VERIFY_ALL_LOG_FILES[$idx]}" 2>&1
      exit_code=$?
      printf '\n' >> "${VERIFY_ALL_LOG_FILES[$idx]}"
      printf '%s\n' "$exit_code" > "$status_tmp"
      mv "$status_tmp" "$status_file"
      exit "$exit_code"
    ) &
    VERIFY_ALL_PIDS[$idx]=$!
  done

  while ((completed < count)); do
    for ((idx = 0; idx < count; idx++)); do
      verify_all_drain_log "$idx"

      if [[ "${VERIFY_ALL_STATES[$idx]}" == "running" ]] \
        && [[ -f "${VERIFY_ALL_STATUS_FILES[$idx]}" ]]; then
        exit_code="$(cat "${VERIFY_ALL_STATUS_FILES[$idx]}")"
        wait "${VERIFY_ALL_PIDS[$idx]}" 2>/dev/null || true
      elif [[ "${VERIFY_ALL_STATES[$idx]}" == "running" ]] \
        && ! kill -0 "${VERIFY_ALL_PIDS[$idx]}" 2>/dev/null; then
        set +e
        wait "${VERIFY_ALL_PIDS[$idx]}" 2>/dev/null
        exit_code=$?
        set -e
      else
        continue
      fi

      if [[ "${VERIFY_ALL_STATES[$idx]}" == "running" ]]; then
        # The completion marker is written after the log, but it can appear
        # between the poll's first log drain and this completion check.
        verify_all_drain_log "$idx"
        VERIFY_ALL_EXIT_CODES[$idx]="$exit_code"
        VERIFY_ALL_STATES[$idx]="done"
        completed=$((completed + 1))

        if ((exit_code == 0)); then
          ui_set_slot "$idx" "pass" "${VERIFY_ALL_LABELS[$idx]} passed"
        else
          failed=$((failed + 1))
          ui_set_slot "$idx" "fail" "${VERIFY_ALL_LABELS[$idx]} failed (exit $exit_code)"
        fi
      fi
    done

    ui_flush_rolling_slots
    if ((completed < count)); then
      sleep 0.1
    fi
  done

  ui_flush_rolling_slots
  verify_all_print_failed_logs
  ui_clear_all_slots
  ui_set_render_mode "task_only"
  ui_clear_live_state

  for ((idx = 0; idx < count; idx++)); do
    if [[ "${VERIFY_ALL_EXIT_CODES[$idx]}" == "0" ]]; then
      release_pass "${VERIFY_ALL_LABELS[$idx]} verification passed"
    else
      fail "${VERIFY_ALL_LABELS[$idx]} verification failed (exit ${VERIFY_ALL_EXIT_CODES[$idx]})"
    fi
  done

  rm -rf -- "$VERIFY_ALL_TEMP_DIR"
  VERIFY_ALL_TEMP_DIR=""
  if ((failed == 0)); then
    return 0
  fi
  return 1
}

verify_all_main() {
  local command_path

  if [[ "${1:-}" == "--help" ]] || [[ "${1:-}" == "-h" ]]; then
    verify_all_usage
    return 0
  fi
  if (($# != 0)); then
    verify_all_usage >&2
    return 2
  fi

  release_ui_initialize "[verify-all]" "Verify all release environments"
  trap 'verify_all_finalize' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  verify_all_configure_default_commands
  if ((${#VERIFY_ALL_COMMANDS[@]} == 0)); then
    release_die "This system supports no local verifiers; use macOS, Windows, or a responsive Docker Linux engine."
  fi
  for command_path in "${VERIFY_ALL_COMMANDS[@]}"; do
    verify_all_validate_command "$command_path"
  done

  release_initialize "$SCRIPT_DIR"
  cd "$RELEASE_REPO_ROOT"
  release_require_tracked_clean
  release_require_current_commit_on_origin

  if ! verify_all_run_parallel; then
    release_die "One or more local verifiers failed."
  fi

  release_pass "All local release environments passed for $RELEASE_SHA"
}

verify_all_finalize() {
  local exit_code=$?

  trap - EXIT INT TERM
  verify_all_cleanup
  release_ui_finalize
  exit "$exit_code"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  verify_all_main "$@"
fi
