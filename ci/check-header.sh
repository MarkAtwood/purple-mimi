#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Regenerates include/purple-mimi.h from the purple-mimi-ffi crate and
# fails if the committed header differs (docs/design/purple-mimi.md §5.5).
# The header is committed so that a build needs neither cbindgen nor the
# network; this keeps it honest.
#
#   ci/check-header.sh           compare, fail on a difference
#   ci/check-header.sh --write   regenerate in place
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
header="$root/include/purple-mimi.h"
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

cbindgen --quiet --config "$root/ffi/cbindgen.toml" \
    --crate purple-mimi-ffi --output "$tmp" --lockfile "$root/Cargo.lock"

# cbindgen copies rustdoc verbatim, so intra-doc links arrive as [`name`].
# Plain backticks read better in a C header. One deterministic pass, run
# the same way on both sides of the comparison.
sed -i.bak 's/\[`\([A-Za-z0-9_]*\)`\]/`\1`/g' "$tmp"
rm -f "$tmp.bak"

if [ "${1-}" = "--write" ]; then
    mkdir -p "$root/include"
    mv "$tmp" "$header"
    trap - EXIT
    echo "wrote $header"
    exit 0
fi

if ! diff -u "$header" "$tmp"; then
    echo >&2
    echo "include/purple-mimi.h is out of date; run ci/check-header.sh --write" >&2
    exit 1
fi
echo "include/purple-mimi.h is up to date"
