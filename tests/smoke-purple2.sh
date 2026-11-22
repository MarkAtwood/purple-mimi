#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Loads a built libpurple 2 plugin in a real headless libpurple and
# checks it registers its protocol (docs/design/purple-mimi.md §11).
#
#   tests/smoke-purple2.sh build/libpurple-mimi.so
#
# Compiles tests/smoke-purple2.c against the system libpurple, puts the
# plugin alone in a directory, and runs the host there. No network, no
# account, no provider.
set -eu

plugin=${1:?usage: smoke-purple2.sh <path to libpurple-mimi.so>}
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

if [ ! -f "$plugin" ]; then
    echo "no such plugin: $plugin" >&2
    exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

mkdir -p "$work/plugins" "$work/home"
cp "$plugin" "$work/plugins/"

echo "building the headless host"
cc -o "$work/smoke" "$root/tests/smoke-purple2.c" \
   $(pkg-config --cflags --libs purple glib-2.0) -Wall -Wextra

echo "running it with the plugin in $work/plugins"
# A real libpurple probes, dlopens and loads the plugin here.
"$work/smoke" "$work/plugins" "$work/home"

# The run must not have left anything outside its own directory, and must
# not have made a state directory: no account was opened.
if [ -d "$work/home/mimi" ]; then
    echo "FAIL: a MIMI state directory was made without an account" >&2
    exit 1
fi
echo "OK: the smoke test passed"
