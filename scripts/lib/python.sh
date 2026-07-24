#!/usr/bin/env bash

# Resolve a working Python 3 interpreter instead of trusting command discovery
# alone. Windows App Execution Aliases can appear on PATH as python3 even when
# launching that command only prints a Microsoft Store prompt and exits.
lfscloud_find_python3() {
    local candidate
    local resolved

    for candidate in python3 python; do
        resolved="$(command -v "$candidate" 2>/dev/null || true)"
        [[ -n "$resolved" ]] || continue

        if "$resolved" -c \
            'import sys; sys.exit(0 if sys.version_info[0] >= 3 else 1)' \
            >/dev/null 2>&1; then
            printf '%s\n' "$resolved"
            return 0
        fi
    done

    return 1
}
