#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
temp_root=$(mktemp -d)
trap 'rm -rf "$temp_root"' EXIT
fixture="$temp_root/runner fixtures"
mkdir -p "$fixture/fake-bin" "$fixture/runner-one" "$fixture/runner-two"
export DX_INSTALL_LOG="$fixture/install.log"
export PATH="$fixture/fake-bin:$PATH"

# Stand in only for cargo's network/compiler work; exercise the actual cache
# activation script and actual executable files across distinct runner homes.
cat > "$fixture/fake-bin/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == install && "$2" == dioxus-cli@0.7.10 ]]
printf '%s\n' "$*" >> "$DX_INSTALL_LOG"
[[ "${DX_INSTALL_FAIL:-0}" != 1 ]] || exit 42
shift 2
while [[ $# -gt 0 ]]; do
    if [[ "$1" == --root ]]; then root="$2"; shift 2; else shift; fi
done
mkdir -p "$root/bin"
printf '#!/usr/bin/env bash\necho "dioxus 0.7.10 (fixture)"\n' > "$root/bin/dx"
chmod +x "$root/bin/dx"
SH
chmod +x "$fixture/fake-bin/cargo"

run_for() {
    local runner="$1"
    (cd "$fixture/$runner" && CARGO_HOME="$fixture/$runner/cargo" bash "$repo_root/.github/scripts/ensure-dioxus-cli.sh")
}
install_count() { wc -l < "$DX_INSTALL_LOG" | tr -d ' '; }

# Cold build installs once and populates the portable cache.
run_for runner-one
[[ "$(install_count)" == 1 ]]

# GitHub restores the same relative path under another workspace/runner.
cp -R "$fixture/runner-one/.ci-tools" "$fixture/runner-two/.ci-tools"
run_for runner-two
[[ "$(install_count)" == 1 ]]
[[ "$("$fixture/runner-two/cargo/bin/dx" --version)" == 'dioxus 0.7.10 (fixture)' ]]

# Missing cache can reuse a correctly preinstalled isolated binary.
rm -rf "$fixture/runner-two/.ci-tools"
run_for runner-two
[[ "$(install_count)" == 1 ]]

# Wrong cached AND installed versions must install the pinned version.
printf '#!/usr/bin/env bash\necho "dioxus 0.7.100"\n' > "$fixture/runner-two/.ci-tools/dioxus/bin/dx"
cp "$fixture/runner-two/.ci-tools/dioxus/bin/dx" "$fixture/runner-two/cargo/bin/dx"
run_for runner-two
[[ "$(install_count)" == 2 ]]

# Failed installation cannot be reported as a successful cache activation.
rm -rf "$fixture/runner-two/.ci-tools" "$fixture/runner-two/cargo"
if DX_INSTALL_FAIL=1 run_for runner-two; then
    echo 'failed installation was incorrectly accepted' >&2
    exit 1
fi
[[ ! -e "$fixture/runner-two/cargo/bin/dx" ]]
echo 'Dioxus CLI cache checks passed'
