#!/usr/bin/env bash

UI_PREFIX="[script]"
LIVE_REGION_ENABLED=false
LIVE_RENDER_MODE="section_task"
LIVE_SECTION_MESSAGE=""
LIVE_SECTION_STATE="running"
LIVE_TASK_MESSAGE=""
LIVE_TASK_STATE="running"
LIVE_REGION_LINES=0
LIVE_SLOT_COUNT=0
LIVE_SLOT_STATES=()
LIVE_SLOT_MESSAGES=()
LIVE_STDOUT_MAX_LINES=5
LIVE_STDOUT_LINE_COUNT=0
LIVE_STDOUT_LINES=()
LIVE_STDOUT_RENDERED_COUNT=0
LIVE_STDOUT_DEFER_RENDER=false
LIVE_STDOUT_PENDING_LINES=0
LIVE_STDOUT_FLUSH_EVERY_LINES=8
UI_TERMINAL_COLUMNS=80

RED=''
GREEN=''
YELLOW=''
BLUE=''
MAGENTA=''
GRAY=''
BOLD=''
RESET=''

ui_set_prefix() {
    UI_PREFIX="$1"
}

ui_set_render_mode() {
    case "$1" in
        section_task|task_only|slots)
            LIVE_RENDER_MODE="$1"
            ;;
        *)
            LIVE_RENDER_MODE="section_task"
            ;;
    esac
}

ui_init() {
    if [[ -n "${NO_COLOR:-}" ]]; then
        RED=''
        GREEN=''
        YELLOW=''
        BLUE=''
        GRAY=''
        BOLD=''
        RESET=''
    elif [[ -n "${FORCE_COLOR:-}" ]] || [[ -n "${CLICOLOR_FORCE:-}" ]] || [[ -t 1 ]]; then
        RED='\033[0;31m'
        GREEN='\033[0;32m'
        YELLOW='\033[0;33m'
        BLUE='\033[0;34m'
        MAGENTA='\033[0;35m'
        GRAY='\033[0;90m'
        BOLD='\033[1m'
        RESET='\033[0m'
    else
        RED=''
        GREEN=''
        YELLOW=''
        BLUE=''
        MAGENTA=''
        GRAY=''
        BOLD=''
        RESET=''
    fi

    if [[ -n "${DUPEMEDIA_LIVE_REGION_FORCE:-}" ]]; then
        LIVE_REGION_ENABLED=true
    elif [[ -z "${NO_COLOR:-}" ]] && [[ ( -n "${FORCE_COLOR:-}" || -n "${CLICOLOR_FORCE:-}" ) ]] && [[ "${TERM:-}" != "dumb" ]]; then
        LIVE_REGION_ENABLED=true
    elif [[ -t 1 ]] && [[ "${TERM:-}" != "dumb" ]]; then
        LIVE_REGION_ENABLED=true
    else
        LIVE_REGION_ENABLED=false
    fi

    if [[ -n "${DUPEMEDIA_LIVE_FLUSH_LINES:-}" ]] && [[ "${DUPEMEDIA_LIVE_FLUSH_LINES}" =~ ^[0-9]+$ ]] && (( DUPEMEDIA_LIVE_FLUSH_LINES > 0 )); then
        LIVE_STDOUT_FLUSH_EVERY_LINES="$DUPEMEDIA_LIVE_FLUSH_LINES"
    else
        LIVE_STDOUT_FLUSH_EVERY_LINES=8
    fi

    ui_refresh_terminal_columns
}

ui_refresh_terminal_columns() {
    local columns=""

    # Prefer querying the controlling terminal directly when one exists.
    # Redirecting from /dev/tty without a controlling terminal can emit a shell
    # error before stty runs, so guard the redirection path explicitly.
    if [[ -t 1 ]] && [[ -r /dev/tty ]] && command -v stty >/dev/null 2>&1; then
        columns="$(stty size < /dev/tty 2>/dev/null | awk '{print $2}' || true)"
        if ! [[ "$columns" =~ ^[0-9]+$ ]] || (( columns <= 0 )); then
            columns="$(stty size 2>/dev/null | awk '{print $2}' || true)"
        fi
    elif command -v stty >/dev/null 2>&1; then
        columns="$(stty size 2>/dev/null | awk '{print $2}' || true)"
    fi

    if ! [[ "$columns" =~ ^[0-9]+$ ]] || (( columns <= 0 )); then
        if command -v tput >/dev/null 2>&1; then
            columns="$(tput cols 2>/dev/null || true)"
        fi
    fi

    if ! [[ "$columns" =~ ^[0-9]+$ ]] || (( columns <= 0 )); then
        columns="${COLUMNS:-}"
    fi

    if ! [[ "$columns" =~ ^[0-9]+$ ]] || (( columns <= 0 )); then
        columns=80
    fi

    UI_TERMINAL_COLUMNS="$columns"
}

ui_clear_live_region() {
    if [[ "$LIVE_REGION_ENABLED" != true ]] || (( LIVE_REGION_LINES == 0 )); then
        return
    fi

    printf '\033[1A'

    local i
    for (( i = 1; i <= LIVE_REGION_LINES; i++ )); do
        printf '\r\033[2K'
        if (( i < LIVE_REGION_LINES )); then
            printf '\033[1A'
        fi
    done

    LIVE_REGION_LINES=0
}

ui_format_line() {
    local state="$1"
    local message="$2"
    local symbol=""
    local label=""
    local color=""

    case "$state" in
        running)
            symbol="▶"
            label="RUN "
            color="$BLUE"
            ;;
        pass)
            symbol="✓"
            label="PASS"
            color="$GREEN"
            ;;
        fail)
            symbol="✗"
            label="FAIL"
            color="$RED"
            ;;
        warn)
            symbol="⚠"
            label="WARN"
            color="$YELLOW"
            ;;
        skip)
            symbol="○"
            label="SKIP"
            color="$MAGENTA"
            ;;
        info|*)
            symbol="ℹ"
            label="INFO"
            color="$BLUE"
            ;;
    esac

    # Defensive hard reset: child commands can leak ANSI color state without
    # sending a trailing reset code. Emit an unconditional reset before each
    # formatted line so our script-owned output always starts from a clean
    # style state, including when NO_COLOR disables our own color wrappers.
    if [[ -t 1 ]]; then
        printf '\033[0m'
    fi

    if [[ -n "$color" ]]; then
        printf '%b%s%b %b%s %s%b %s\n' "$GRAY" "$UI_PREFIX" "$RESET" "$color" "$symbol" "$label" "$RESET" "$message"
    else
        printf '%s %s %s %s\n' "$UI_PREFIX" "$symbol" "$label" "$message"
    fi
}

ui_format_child_task_line() {
    local state="$1"
    local message="$2"
    local symbol=""
    local label=""
    local color=""

    case "$state" in
        running)
            symbol="▸"
            label="TASK"
            color="$BLUE"
            ;;
        pass)
            symbol="✓"
            label="DONE"
            color="$GREEN"
            ;;
        fail)
            symbol="✗"
            label="FAIL"
            color="$RED"
            ;;
        warn)
            symbol="⚠"
            label="WARN"
            color="$YELLOW"
            ;;
        skip)
            symbol="○"
            label="SKIP"
            color="$MAGENTA"
            ;;
        info|*)
            symbol="ℹ"
            label="INFO"
            color="$BLUE"
            ;;
    esac

    if [[ -t 1 ]]; then
        printf '\033[0m'
    fi

    if [[ -n "$color" ]]; then
        printf '%b%s%b %b└─%b %b%s %s%b %s\n' "$GRAY" "$UI_PREFIX" "$RESET" "$GRAY" "$RESET" "$color" "$symbol" "$label" "$RESET" "$message"
    else
        printf '%s └─ %s %s %s\n' "$UI_PREFIX" "$symbol" "$label" "$message"
    fi
}

ui_render_live_region() {
    if [[ "$LIVE_REGION_ENABLED" != true ]]; then
        return
    fi

    ui_refresh_terminal_columns

    local lines=0

    if [[ "$LIVE_RENDER_MODE" == "slots" ]]; then
        local idx
        for (( idx = 0; idx < LIVE_SLOT_COUNT; idx++ )); do
            local slot_message="${LIVE_SLOT_MESSAGES[$idx]:-}"
            if [[ -n "$slot_message" ]]; then
                slot_message="$(ui_fit_live_status_message "$slot_message")"
                ui_format_line "${LIVE_SLOT_STATES[$idx]:-running}" "$slot_message"
                lines=$((lines + 1))
            fi
        done
    elif [[ "$LIVE_RENDER_MODE" == "task_only" ]]; then
        local task_visible=false
        if [[ -n "$LIVE_TASK_MESSAGE" ]]; then
            local task_line
            task_line="$(ui_fit_live_status_message "$LIVE_TASK_MESSAGE")"
            ui_format_line "$LIVE_TASK_STATE" "$task_line"
            lines=$((lines + 1))
            task_visible=true
        elif [[ -n "$LIVE_SECTION_MESSAGE" ]]; then
            local section_line
            section_line="$(ui_fit_live_status_message "$LIVE_SECTION_MESSAGE")"
            ui_format_line "$LIVE_SECTION_STATE" "$section_line"
            lines=$((lines + 1))
        fi

        if [[ "$task_visible" == true ]] && (( LIVE_STDOUT_LINE_COUNT > 0 )); then
            local stdout_idx
            for (( stdout_idx = 0; stdout_idx < LIVE_STDOUT_LINE_COUNT; stdout_idx++ )); do
                local stdout_line="${LIVE_STDOUT_LINES[$stdout_idx]:-}"
                if [[ -n "$GRAY" ]]; then
                    printf '%b  │ %s%b\n' "$GRAY" "$stdout_line" "$RESET"
                else
                    printf '  | %s\n' "$stdout_line"
                fi
                lines=$((lines + 1))
            done
        fi
    else
        if [[ -n "$LIVE_SECTION_MESSAGE" ]]; then
            local section_line
            section_line="$(ui_fit_live_status_message "$LIVE_SECTION_MESSAGE")"
            ui_format_line "$LIVE_SECTION_STATE" "$section_line"
            lines=$((lines + 1))
        fi

        if [[ -n "$LIVE_TASK_MESSAGE" ]]; then
            local child_task_line
            child_task_line="$(ui_fit_live_status_message "$LIVE_TASK_MESSAGE")"
            ui_format_child_task_line "$LIVE_TASK_STATE" "$child_task_line"
            lines=$((lines + 1))

            if (( LIVE_STDOUT_LINE_COUNT > 0 )); then
                local stdout_idx
                for (( stdout_idx = 0; stdout_idx < LIVE_STDOUT_LINE_COUNT; stdout_idx++ )); do
                    local stdout_line="${LIVE_STDOUT_LINES[$stdout_idx]:-}"
                    if [[ -n "$GRAY" ]]; then
                        printf '%b   │ %s%b\n' "$GRAY" "$stdout_line" "$RESET"
                    else
                        printf '   | %s\n' "$stdout_line"
                    fi
                    lines=$((lines + 1))
                done
            fi
        fi
    fi

    LIVE_REGION_LINES=$lines

    if ui_live_stdout_is_visible && (( LIVE_STDOUT_LINE_COUNT > 0 )); then
        LIVE_STDOUT_RENDERED_COUNT=$LIVE_STDOUT_LINE_COUNT
    else
        LIVE_STDOUT_RENDERED_COUNT=0
    fi
}

ui_live_stdout_reset() {
    if (( LIVE_STDOUT_LINE_COUNT == 0 )); then
        return
    fi

    LIVE_STDOUT_LINES=()
    LIVE_STDOUT_LINE_COUNT=0
    LIVE_STDOUT_RENDERED_COUNT=0
    LIVE_STDOUT_PENDING_LINES=0
    ui_clear_live_region
    ui_render_live_region
}

ui_fit_live_stream_line() {
    local line="$1"
    local columns
    columns="$(ui_terminal_columns)"

    # Reserve space for stream prefixes like "  │ " / "   │ " plus a safety
    # margin so lines never hard-wrap and desynchronize live-region collapse.
    local max_len=$((columns - 6))
    if (( max_len < 20 )); then
        max_len=20
    fi

    if (( ${#line} > max_len )); then
        printf '%s…\n' "${line:0:max_len-1}"
    else
        printf '%s\n' "$line"
    fi
}

ui_fit_live_status_message() {
    local message="$1"
    local columns
    columns="$(ui_terminal_columns)"

    local max_len=$((columns - 24))
    if (( max_len < 20 )); then
        max_len=20
    fi

    if (( ${#message} > max_len )); then
        printf '%s' "${message:0:max_len-1}…"
    else
        printf '%s' "$message"
    fi
}

ui_terminal_columns() {
    # Return cached width to keep hot output paths fast.
    printf '%s' "$UI_TERMINAL_COLUMNS"
}

ui_live_stdout_is_visible() {
    if [[ "$LIVE_RENDER_MODE" == "slots" ]]; then
        return 1
    fi

    if [[ -z "$LIVE_TASK_MESSAGE" ]]; then
        return 1
    fi

    return 0
}

ui_print_live_stdout_line() {
    local stdout_line="$1"

    if [[ "$LIVE_RENDER_MODE" == "task_only" ]]; then
        printf '  │ %s\n' "$stdout_line"
    else
        printf '   │ %s\n' "$stdout_line"
    fi
}

ui_redraw_live_stdout_block() {
    local lines="$LIVE_STDOUT_LINE_COUNT"
    if (( lines == 0 )); then
        return
    fi

    # Repaint only the command-output block (tail window), preserving section/task
    # lines above and avoiding full live-region clear/redraw flicker.
    printf '\033[%dA' "$lines"

    local idx
    for (( idx = 0; idx < lines; idx++ )); do
        printf '\r\033[2K'
        ui_print_live_stdout_line "${LIVE_STDOUT_LINES[$idx]:-}"
    done
}

ui_live_stdout_flush_render() {
    local current_count="$LIVE_STDOUT_LINE_COUNT"
    local rendered_count="$LIVE_STDOUT_RENDERED_COUNT"

    if (( current_count == 0 )); then
        LIVE_STDOUT_RENDERED_COUNT=0
        LIVE_STDOUT_PENDING_LINES=0
        return
    fi

    if [[ "$LIVE_REGION_ENABLED" != true ]] || ! ui_live_stdout_is_visible; then
        ui_clear_live_region
        ui_render_live_region
        LIVE_STDOUT_RENDERED_COUNT="$LIVE_STDOUT_LINE_COUNT"
        LIVE_STDOUT_PENDING_LINES=0
        return
    fi

    ui_refresh_terminal_columns

    if (( rendered_count == 0 )); then
        local idx
        for (( idx = 0; idx < current_count; idx++ )); do
            ui_print_live_stdout_line "${LIVE_STDOUT_LINES[$idx]:-}"
        done
        LIVE_REGION_LINES=$((LIVE_REGION_LINES + current_count))
        LIVE_STDOUT_RENDERED_COUNT="$current_count"
        LIVE_STDOUT_PENDING_LINES=0
        return
    fi

    if (( current_count > rendered_count )); then
        local idx
        for (( idx = rendered_count; idx < current_count; idx++ )); do
            ui_print_live_stdout_line "${LIVE_STDOUT_LINES[$idx]:-}"
        done
        LIVE_REGION_LINES=$((LIVE_REGION_LINES + (current_count - rendered_count)))
        LIVE_STDOUT_RENDERED_COUNT="$current_count"
        LIVE_STDOUT_PENDING_LINES=0
        return
    fi

    ui_redraw_live_stdout_block
    LIVE_STDOUT_RENDERED_COUNT="$current_count"
    LIVE_STDOUT_PENDING_LINES=0
}

ui_live_stdout_should_flush() {
    if (( LIVE_STDOUT_LINE_COUNT > 0 && LIVE_STDOUT_RENDERED_COUNT == 0 )); then
        return 0
    fi

    if (( LIVE_STDOUT_PENDING_LINES >= LIVE_STDOUT_FLUSH_EVERY_LINES )); then
        return 0
    fi

    return 1
}

_ui_live_stdout_append_clean_line() {
    local line="$1"

    if [[ -z "${line//[[:space:]]/}" ]]; then
        return
    fi

    line="$(ui_fit_live_stream_line "$line")"

    if (( LIVE_STDOUT_LINE_COUNT < LIVE_STDOUT_MAX_LINES )); then
        LIVE_STDOUT_LINES[$LIVE_STDOUT_LINE_COUNT]="$line"
        LIVE_STDOUT_LINE_COUNT=$((LIVE_STDOUT_LINE_COUNT + 1))
    else
        local idx
        for (( idx = 0; idx < LIVE_STDOUT_MAX_LINES - 1; idx++ )); do
            LIVE_STDOUT_LINES[$idx]="${LIVE_STDOUT_LINES[$((idx + 1))]}"
        done
        LIVE_STDOUT_LINES[$((LIVE_STDOUT_MAX_LINES - 1))]="$line"
    fi

    LIVE_STDOUT_PENDING_LINES=$((LIVE_STDOUT_PENDING_LINES + 1))
}

ui_live_stdout_append_line() {
    local raw_line="$1"
    local normalized

    # Preserve ANSI color/style sequences from child command output so the
    # rolling live stdout window reflects original colors. Normalize carriage
    # returns into line breaks to avoid CR-based progress updates overwriting.
    normalized="${raw_line//$'\r'/$'\n'}"

    local segment
    while IFS= read -r segment; do
        _ui_live_stdout_append_clean_line "$segment"
    done <<< "$normalized"

    if [[ "$LIVE_STDOUT_DEFER_RENDER" != true ]] || ui_live_stdout_should_flush; then
        ui_live_stdout_flush_render
    fi
}

ui_run_with_live_stdout() {
    if [[ "$LIVE_REGION_ENABLED" != true ]]; then
        "$@"
        return $?
    fi

    local stdout_fifo
    local stdout_fifo_dir
    local stdout_capture
    local stderr_capture
    local command_pid
    local command_status=0

    _ui_print_deferred_output() {
        local message="$1"
        local fallback_color="$2"

        [[ -n "$message" ]] || return 0

        ui_pause_live_region_for_command_output
        if [[ "$message" == *$'\033'* ]]; then
            printf '%s\n' "$message" >&2
        else
            printf '%b%s%b\n' "$fallback_color" "$message" "$RESET" >&2
        fi
    }

    stdout_fifo_dir="$(mktemp -d)"
    stdout_fifo="$stdout_fifo_dir/stdout.fifo"
    stdout_capture="$(mktemp)"
    stderr_capture="$(mktemp)"
    mkfifo "$stdout_fifo"

    ui_live_stdout_reset
    LIVE_STDOUT_DEFER_RENDER=true

    "$@" > "$stdout_fifo" 2> "$stderr_capture" &
    command_pid=$!

    local line
    while IFS= read -r line || [[ -n "$line" ]]; do
        printf '%s\n' "$line" >> "$stdout_capture"
        ui_live_stdout_append_line "$line"

        if ui_live_stdout_should_flush; then
            ui_live_stdout_flush_render
        fi
    done < "$stdout_fifo"

    LIVE_STDOUT_DEFER_RENDER=false
    ui_live_stdout_flush_render

    if wait "$command_pid"; then
        command_status=0
    else
        command_status=$?
    fi

    rm -f "$stdout_fifo"
    rmdir "$stdout_fifo_dir" 2>/dev/null || true

    local captured_stderr
    captured_stderr="$(cat "$stderr_capture")"

    # Collapse the current live region at command completion.
    if (( LIVE_STDOUT_LINE_COUNT > 0 )); then
        ui_clear_live_region

        LIVE_STDOUT_LINES=()
        LIVE_STDOUT_LINE_COUNT=0
        LIVE_STDOUT_RENDERED_COUNT=0
        LIVE_STDOUT_PENDING_LINES=0
    fi

    if (( command_status != 0 )); then
        if [[ -n "$captured_stderr" ]]; then
            _ui_print_deferred_output "$captured_stderr" "$YELLOW"
        fi

        local captured_output
        captured_output="$(cat "$stdout_capture")"
        if [[ -n "$captured_output" ]]; then
            _ui_print_deferred_output "$captured_output" "$YELLOW"
        fi
    fi

    rm -f "$stdout_capture" "$stderr_capture"
    return "$command_status"
}

ui_enable_slots() {
    local count="$1"
    local idx

    LIVE_SLOT_COUNT="$count"
    LIVE_SLOT_STATES=()
    LIVE_SLOT_MESSAGES=()

    for (( idx = 0; idx < LIVE_SLOT_COUNT; idx++ )); do
        LIVE_SLOT_STATES[$idx]="running"
        LIVE_SLOT_MESSAGES[$idx]=""
    done

    LIVE_RENDER_MODE="slots"
    ui_clear_live_region
    ui_render_live_region
}

ui_slots_all_visible() {
    if (( LIVE_SLOT_COUNT == 0 )); then
        return 1
    fi

    local idx
    for (( idx = 0; idx < LIVE_SLOT_COUNT; idx++ )); do
        if [[ -z "${LIVE_SLOT_MESSAGES[$idx]:-}" ]]; then
            return 1
        fi
    done

    return 0
}

ui_update_slot_in_place() {
    local idx="$1"

    if [[ "$LIVE_REGION_ENABLED" != true ]] || [[ "$LIVE_RENDER_MODE" != "slots" ]]; then
        return 1
    fi

    if (( LIVE_SLOT_COUNT == 0 )) || (( idx < 0 )) || (( idx >= LIVE_SLOT_COUNT )); then
        return 1
    fi

    # In-place slot updates require a fixed visible slot region where each slot
    # occupies exactly one rendered line.
    if (( LIVE_REGION_LINES != LIVE_SLOT_COUNT )); then
        return 1
    fi

    if ! ui_slots_all_visible; then
        return 1
    fi

    local slot_message="${LIVE_SLOT_MESSAGES[$idx]:-}"
    local slot_state="${LIVE_SLOT_STATES[$idx]:-running}"
    slot_message="$(ui_fit_live_status_message "$slot_message")"

    local lines_up=$((LIVE_SLOT_COUNT - idx))
    local lines_down=$((LIVE_SLOT_COUNT - idx - 1))

    printf '\033[%dA' "$lines_up"
    printf '\r\033[2K'
    ui_format_line "$slot_state" "$slot_message"

    if (( lines_down > 0 )); then
        printf '\033[%dB' "$lines_down"
    fi

    return 0
}

ui_set_slot() {
    local idx="$1"
    local state="$2"
    local message="$3"

    LIVE_SLOT_STATES[$idx]="$state"
    LIVE_SLOT_MESSAGES[$idx]="$message"

    if ! ui_update_slot_in_place "$idx"; then
        ui_clear_live_region
        ui_render_live_region
    fi
}

ui_clear_slot() {
    local idx="$1"

    LIVE_SLOT_STATES[$idx]="running"
    LIVE_SLOT_MESSAGES[$idx]=""
    ui_clear_live_region
    ui_render_live_region
}

ui_clear_all_slots() {
    local idx
    for (( idx = 0; idx < LIVE_SLOT_COUNT; idx++ )); do
        LIVE_SLOT_STATES[$idx]="running"
        LIVE_SLOT_MESSAGES[$idx]=""
    done
    ui_clear_live_region
    ui_render_live_region
}

ui_set_live_section_running() {
    local message="$1"
    LIVE_SECTION_MESSAGE="$message"
    LIVE_SECTION_STATE="running"
    ui_clear_live_region
    ui_render_live_region
}

ui_set_live_task_state() {
    local state="$1"
    local message="$2"
    LIVE_TASK_STATE="$state"
    LIVE_TASK_MESSAGE="$message"
    ui_clear_live_region
    ui_render_live_region
}

ui_clear_live_task() {
    LIVE_TASK_MESSAGE=""
    LIVE_TASK_STATE="running"
    LIVE_STDOUT_LINES=()
    LIVE_STDOUT_LINE_COUNT=0
    LIVE_STDOUT_RENDERED_COUNT=0
    LIVE_STDOUT_PENDING_LINES=0
    ui_clear_live_region
    ui_render_live_region
}

ui_clear_live_state() {
    LIVE_SECTION_MESSAGE=""
    LIVE_SECTION_STATE="running"
    LIVE_TASK_MESSAGE=""
    LIVE_TASK_STATE="running"
    LIVE_STDOUT_LINES=()
    LIVE_STDOUT_LINE_COUNT=0
    LIVE_STDOUT_RENDERED_COUNT=0
    LIVE_STDOUT_PENDING_LINES=0
    ui_clear_live_region
}

ui_pause_live_region_for_command_output() {
    if [[ "$LIVE_REGION_ENABLED" != true ]]; then
        return
    fi

    ui_clear_live_region
}

ui_resume_live_region_after_command_output() {
    if [[ "$LIVE_REGION_ENABLED" != true ]]; then
        return
    fi

    ui_render_live_region
}

ui_log_persistent() {
    local state="$1"
    local message="$2"
    ui_clear_live_region
    if [[ -t 1 ]]; then
        printf '\r\033[2K'
    fi
    ui_format_line "$state" "$message"
    ui_render_live_region
}

ui_log_persistent_raw() {
    local message="$1"
    local fallback_color="${2-$YELLOW}"

    ui_pause_live_region_for_command_output
    if [[ -t 1 ]]; then
        printf '\033[0m' >&2
    fi
    if [[ "$message" == *$'\033'* ]]; then
        printf '%s\n' "$message" >&2
    else
        printf '%b%s%b\n' "$fallback_color" "$message" "$RESET" >&2
    fi
    ui_resume_live_region_after_command_output
}

ui_log_persistent_raw_batch() {
    local message="$1"
    local fallback_color="${2-$YELLOW}"

    [[ -n "$message" ]] || return 0

    ui_pause_live_region_for_command_output

    local line
    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ -t 1 ]]; then
            printf '\033[0m' >&2
        fi
        if [[ "$line" == *$'\033'* ]]; then
            printf '%s\n' "$line" >&2
        else
            printf '%b%s%b\n' "$fallback_color" "$line" "$RESET" >&2
        fi
    done <<< "$message"

    ui_resume_live_region_after_command_output
}

ui_log_persistent_tsv_batch() {
    local message="$1"

    [[ -n "$message" ]] || return 0

    ui_pause_live_region_for_command_output

    local line
    while IFS= read -r line || [[ -n "$line" ]]; do
        [[ -n "$line" ]] || continue

        local state="${line%%$'\t'*}"
        local rendered_message="${line#*$'\t'}"

        if [[ "$line" != *$'\t'* ]]; then
            state="info"
            rendered_message="$line"
        fi

        case "$state" in
            warn|fail)
                ui_format_line "$state" "$rendered_message" >&2
                ;;
            pass|skip|running|info)
                ui_format_line "$state" "$rendered_message"
                ;;
            *)
                ui_format_line "info" "$rendered_message"
                ;;
        esac
    done <<< "$message"

    ui_resume_live_region_after_command_output
}

ui_force_color_env() {
    unset NO_COLOR
    export TERM="${TERM:-xterm-256color}"
    export CLICOLOR=1
    export CLICOLOR_FORCE=1
    export FORCE_COLOR=1
    export CARGO_TERM_COLOR=always
    export RUST_LOG_STYLE=always
    export AV_LOG_FORCE_COLOR=1
}

ui_finalize() {
    # Best-effort terminal restoration for scripts that use heavy cursor
    # control/live-region rendering.
    ui_clear_live_state

    if [[ -t 1 ]]; then
        # Reset style and ensure cursor is visible.
        printf '\033[0m\033[?25h'
        # Clean current line without forcing an extra trailing blank line.
        printf '\r\033[2K'
    fi

    if [[ -t 0 ]]; then
        stty sane >/dev/null 2>&1 || true
    fi
}

info() {
    ui_log_persistent "info" "$*"
}

warn() {
    ui_log_persistent "warn" "$*" >&2
}

pass() {
    ui_log_persistent "pass" "$*"
}

fail() {
    ui_log_persistent "fail" "$*" >&2
}

skip() {
    ui_log_persistent "skip" "$*"
}
