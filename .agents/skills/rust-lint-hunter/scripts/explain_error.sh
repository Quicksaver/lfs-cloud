#!/bin/bash
set -euo pipefail

# Explain a Rust error code

ERROR_CODE="${1:-}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REF_FILE="${SCRIPT_DIR}/../references/dictionary_of_pain.md"

if [ -z "$ERROR_CODE" ]; then
    echo "Usage: $0 <E-CODE>"
    exit 1
fi

if [[ -f "$REF_FILE" ]] && grep -q "^## ${ERROR_CODE}:" "$REF_FILE"; then
    echo "Entry found for $ERROR_CODE in Dictionary of Pain:"
    echo "---------------------------------------------------"
    grep -A 10 "^## ${ERROR_CODE}:" "$REF_FILE"
else
    echo "No custom entry for $ERROR_CODE. Running standard rustc explain..."
    rustc --explain "$ERROR_CODE" | head -n 20
fi
