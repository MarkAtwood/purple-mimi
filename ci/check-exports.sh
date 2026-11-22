#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# A plugin must export its entry point and nothing else (§10.1): a
# staticlib drags in every Rust symbol and the whole standard library,
# and two Rust plugins in one Pidgin must not resolve each other's.
#
#   ci/check-exports.sh <plugin.so> <expected entry point>...
set -eu

plugin=${1:?usage: check-exports.sh <plugin.so> <symbol>...}
shift

if ! command -v nm >/dev/null 2>&1; then
    echo "nm is not available; skipping the export check" >&2
    exit 0
fi

# Defined, global, dynamic symbols.
actual=$(nm --dynamic --defined-only --extern-only --format=posix "$plugin" \
         | awk '{print $1}' | sort -u)

status=0
for want in "$@"; do
    if ! printf '%s\n' "$actual" | grep -qx "$want"; then
        echo "$plugin does not export $want" >&2
        status=1
    fi
done

# Anything else that is not a linker-generated symbol is a leak.
extra=$(printf '%s\n' "$actual" \
        | grep -vxF "$(printf '%s\n' "$@")" \
        | grep -vE '^(_init|_fini|_edata|_end|__bss_start|__.*)$' || true)
if [ -n "$extra" ]; then
    echo "$plugin exports more than its entry points:" >&2
    printf '%s\n' "$extra" >&2
    status=1
fi

if [ "$status" -eq 0 ]; then
    echo "$plugin exports only: $*"
fi
exit "$status"
