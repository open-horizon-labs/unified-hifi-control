#!/usr/bin/env bash
set -euo pipefail

# Cache this workspace-relative path; fleet runner names must not enter the
# cache version. Activate the tool separately in each runner's isolated home.
tool_root="$PWD/.ci-tools/dioxus"
cargo_bin="${CARGO_HOME:?CARGO_HOME must identify the isolated runner home}/bin"
cli_version="0.7.10"

correct_version() {
    local output name version rest
    output=$("$1" --version 2>/dev/null) || return 1
    read -r name version rest <<< "$output"
    [[ "$name" == "dioxus" && "$version" == "$cli_version" ]]
}

mkdir -p "$tool_root/bin" "$cargo_bin"
if correct_version "$tool_root/bin/dx"; then
    echo "Reusing cached Dioxus CLI $cli_version"
elif correct_version "$cargo_bin/dx"; then
    echo "Reusing installed Dioxus CLI $cli_version"
    install -m 755 "$cargo_bin/dx" "$tool_root/bin/dx"
else
    echo "Installing Dioxus CLI $cli_version"
    cargo install "dioxus-cli@$cli_version" --locked --root "$tool_root" --force
fi

# Validate restored/installed content, not just the cache action's hit flag.
correct_version "$tool_root/bin/dx"
install -m 755 "$tool_root/bin/dx" "$cargo_bin/dx"
correct_version "$cargo_bin/dx"
