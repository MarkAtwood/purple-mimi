#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Cargo applies only the top workspace's [patch.crates-io], so this
# workspace must repeat mimid's OpenMLS patch table exactly, at the same
# revision (docs/design/purple-mimi.md §5.2). If the two drift, this
# workspace builds a different OpenMLS than mimid was tested against.
#
# Compares our [patch.crates-io] with mimid's at the pinned revision.
# mimid's Cargo.toml is fetched from the checkout named by MIMID_DIR when
# it is set, and from the pinned revision in the git cache otherwise, so
# CI needs no network beyond the fetch cargo already did.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

# The revision purple-mimi pins, from our own Cargo.toml.
rev=$(sed -n 's/^mimid-agent-core = .*rev = "\([0-9a-f]*\)".*/\1/p' \
      "$root/Cargo.toml" | head -1)
if [ -z "$rev" ]; then
    echo "could not read the pinned mimid revision from Cargo.toml" >&2
    exit 1
fi
echo "pinned mimid revision: $rev"

# The [patch.crates-io] block of a Cargo.toml, normalised.
patch_table() {
    awk '/^\[patch\.crates-io\]/ {inside=1; next}
         /^\[/ {inside=0}
         inside && NF && $0 !~ /^#/ {print}' "$1" | sort
}

ours=$(mktemp)
theirs=$(mktemp)
trap 'rm -f "$ours" "$theirs"' EXIT

patch_table "$root/Cargo.toml" > "$ours"

if [ -n "${MIMID_DIR-}" ]; then
    mimid_toml="$MIMID_DIR/Cargo.toml"
    echo "comparing with $mimid_toml (MIMID_DIR)"
    if [ ! -f "$mimid_toml" ]; then
        echo "no Cargo.toml in MIMID_DIR=$MIMID_DIR" >&2
        exit 1
    fi
    patch_table "$mimid_toml" > "$theirs"
else
    # The revision cargo already checked out for the git dependency.
    cargo_home=${CARGO_HOME:-$HOME/.cargo}
    found=$(find "$cargo_home/git/checkouts" -maxdepth 3 -type f \
            -name Cargo.toml -path '*mimid*' 2>/dev/null | head -1 || true)
    if [ -z "$found" ]; then
        echo "no mimid checkout under $cargo_home/git/checkouts;" >&2
        echo "run 'cargo fetch' first, or set MIMID_DIR" >&2
        exit 1
    fi
    echo "comparing with $found"
    patch_table "$found" > "$theirs"
fi

if ! diff -u "$theirs" "$ours"; then
    echo >&2
    echo "purple-mimi's [patch.crates-io] differs from mimid's at $rev." >&2
    echo "Copy mimid's table into Cargo.toml (§5.2)." >&2
    exit 1
fi
echo "[patch.crates-io] matches mimid's"
