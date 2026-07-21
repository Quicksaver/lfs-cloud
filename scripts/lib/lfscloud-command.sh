#!/usr/bin/env bash

# Run the exact binary selected by the smoke harness when one is provided.
# Local manual verification keeps the existing Cargo fallback.
run_lfscloud() {
    local project_dir="$1"
    shift

    if [[ -n "${LFS_CLOUD_SMOKE_BINARY:-}" ]]; then
        "${LFS_CLOUD_SMOKE_BINARY}" "$@"
        return
    fi

    cargo run --quiet --manifest-path "$project_dir/Cargo.toml" -- "$@"
}
