#!/bin/sh
# Every test process gets scratch XDG config and state directories, so no test (or the
# herdr binary an integration test spawns) reads or migrates a developer's real Herdr
# state, config or hangar sign-in. Tests that need their own directories still set them.
set -eu
root=$(mktemp -d "${TMPDIR:-/tmp}/herdr-nextest.XXXXXX")
mkdir -p "$root/state" "$root/config"
{
    echo "XDG_STATE_HOME=$root/state"
    echo "XDG_CONFIG_HOME=$root/config"
} >> "$NEXTEST_ENV"
