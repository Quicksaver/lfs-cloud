#!/usr/bin/env bash
# merge.sh — Merge the current branch into a target branch and switch to it.
#
# Usage: ./scripts/git/merge.sh [target-branch]
#
# If [target-branch] is omitted, the script uses the repository default branch
# (resolved from origin/HEAD).
#
# Guards:
#   1. Must NOT already be on target branch
#   2. Working tree must be clean (no unstaged or uncommitted changes)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/terminal-ui.sh
source "$SCRIPT_DIR/../lib/terminal-ui.sh"

ui_set_prefix "[merge]"
ui_set_render_mode "task_only"
ui_init
trap 'ui_finalize' EXIT

die() {
    local message="$1"
    ui_set_live_task_state "fail" "$message"
    ui_clear_live_state
    fail "$message"
    exit 1
}

run_step() {
    local task_label="$1"
    local fail_detail="$2"
    shift 2

    ui_set_live_task_state "running" "$task_label"
    if ui_run_with_live_stdout "$@"; then
        ui_set_live_task_state "pass" "$task_label"
        ui_clear_live_state
        pass "$task_label"
        return 0
    fi

    ui_set_live_task_state "fail" "$task_label"
    ui_clear_live_state
    fail "$task_label"
    if [[ -n "$fail_detail" ]]; then
        fail "$fail_detail"
    fi
    return 1
}

resolve_default_branch() {
    local default_ref

    default_ref="$(git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null || true)"
    if [[ -z "$default_ref" ]]; then
        die "Could not resolve default branch from origin/HEAD. Provide a target branch explicitly."
    fi

    echo "${default_ref#origin/}"
}

if [[ $# -gt 1 ]]; then
    die "Usage: ./scripts/git/merge.sh [target-branch]"
fi

target_branch="${1:-}"
if [[ -z "$target_branch" ]]; then
    target_branch="$(resolve_default_branch)"
fi

# --- Pre-flight checks ---
ui_set_live_section_running "Merge current branch into '$target_branch'"

ui_set_live_task_state "running" "Resolve current branch"
current_branch="$(git symbolic-ref --short HEAD 2>/dev/null)" \
    || die "Not on any branch (detached HEAD?)."
if [[ "$current_branch" == "$target_branch" ]]; then
    die "Already on '$target_branch'. Switch to a feature branch first."
fi
ui_set_live_task_state "pass" "Resolve current branch: $current_branch"
ui_clear_live_state
pass "Resolve current branch: $current_branch"

ui_set_live_task_state "running" "Verify working tree is clean"
if ! git diff --quiet; then
    die "Unstaged changes detected. Stage or stash them first."
fi
if ! git diff --cached --quiet; then
    die "Uncommitted staged changes detected. Commit or stash them first."
fi
if [[ -n "$(git status --porcelain --untracked-files=all 2>/dev/null | grep -E '^\?\?' || true)" ]]; then
    die "Untracked files detected. Commit, clean, or stash them first."
fi
ui_set_live_task_state "pass" "Verify working tree is clean"
ui_clear_live_state
pass "Verify working tree is clean"

# --- Ensure target branch is up-to-date with origin ---
run_step \
    "Fetch latest from origin/$target_branch" \
    "Failed to fetch origin/$target_branch. Check your network connection or branch name." \
    git fetch origin "$target_branch" || exit 1

run_step \
    "Switch to $target_branch" \
    "Failed to switch to $target_branch." \
    git checkout "$target_branch" || exit 1

run_step \
    "Pull latest origin/$target_branch" \
    "Failed to pull origin/$target_branch. Resolve any issues and retry." \
    git pull origin "$target_branch" || exit 1

# --- Merge feature branch into target branch ---
run_step \
    "Merge '$current_branch' into $target_branch" \
    "Merge failed. Resolve conflicts and run 'git merge --continue'." \
    git merge "$current_branch" || exit 1

# --- Push target branch to origin ---
run_step \
    "Push $target_branch to origin" \
    "Failed to push $target_branch to origin." \
    git push origin "$target_branch" || exit 1
