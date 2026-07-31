#!/bin/bash

set -uo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
SYNOLOGY_DIR="${ROOT_DIR}/build/synology"
FAILURES=0

fail() {
    echo "FAIL: $*" >&2
    FAILURES=$((FAILURES + 1))
}

assert_contains() {
    local file=$1
    local pattern=$2
    local message=$3

    if ! grep -Eq -- "$pattern" "$file"; then
        fail "$message"
    fi
}

assert_not_contains() {
    local file=$1
    local pattern=$2
    local message=$3

    if grep -Eq -- "$pattern" "$file"; then
        fail "$message"
    fi
}

icon_dimensions=$(file "${SYNOLOGY_DIR}/PACKAGE_ICON.PNG")
if [[ "$icon_dimensions" != *"64 x 64"* ]]; then
    fail "PACKAGE_ICON.PNG must be 64 x 64 for DSM 7 (got: ${icon_dimensions})"
fi

for script in "${SYNOLOGY_DIR}"/scripts/*; do
    assert_contains "$script" '^#!/bin/sh$' "$(basename "$script") must use DSM's POSIX /bin/sh"
    sh -n "$script" || fail "$(basename "$script") has invalid POSIX shell syntax"
done

control_script="${SYNOLOGY_DIR}/scripts/start-stop-status"
assert_not_contains "$control_script" '/var/(run|log)' "control script must not write to root-owned /var/run or /var/log"
assert_contains "$control_script" 'VAR_DIR="\$\{PACKAGE_DIR\}/var"' "control script must use the package-owned DSM var directory"
assert_contains "$control_script" 'return 3' "status must return 3 when the package is normally stopped"

assert_contains "${ROOT_DIR}/.github/workflows/build.yml" 'arch: x86_64' "Synology x86 package must target the x86_64 platform family"
assert_contains "${ROOT_DIR}/.github/workflows/build.yml" 'arch: armv8' "Synology ARM package must target the armv8 platform family"

if [[ ! -x "${SYNOLOGY_DIR}/build-spk.sh" ]]; then
    fail "build-spk.sh must assemble and validate SPKs reproducibly"
else
    test_tmp=$(mktemp -d "${TMPDIR:-/tmp}/uhc-synology-contract.XXXXXX")
    trap 'rm -rf "$test_tmp"' EXIT

    test_binary="${test_tmp}/unified-hifi-control"
    printf '#!/bin/sh\nexit 0\n' > "$test_binary"
    chmod +x "$test_binary"

    for version_case in '3.4.1:3.4.1-10000' '3.5.0-rc.2:3.5.0-9002' '0.0.0-pr171:0.0.0-0171' '0.0.0-dev:0.0.0-0001'; do
        input_version=${version_case%%:*}
        expected_version=${version_case#*:}
        output_spk="${test_tmp}/${input_version}.spk"

        if ! "${SYNOLOGY_DIR}/build-spk.sh" "$input_version" x86_64 "$test_binary" "$output_spk"; then
            fail "build-spk.sh failed for version ${input_version}"
            continue
        fi

        actual_version=$(tar -xOf "$output_spk" INFO | sed -n 's/^version="\([^"]*\)"$/\1/p')
        [[ "$actual_version" == "$expected_version" ]] || fail "${input_version} normalized to ${actual_version}, expected ${expected_version}"

        actual_arch=$(tar -xOf "$output_spk" INFO | sed -n 's/^arch="\([^"]*\)"$/\1/p')
        [[ "$actual_arch" == 'x86_64' ]] || fail "SPK arch is ${actual_arch}, expected x86_64"

        declared_size=$(tar -xOf "$output_spk" INFO | sed -n 's/^extractsize="\([0-9]*\)"$/\1/p')
        unpack_dir="${test_tmp}/unpack-${input_version}"
        mkdir -p "$unpack_dir"
        tar -xOf "$output_spk" package.tgz | tar -xzf - -C "$unpack_dir"
        actual_size=$(du -sk "$unpack_dir" | cut -f1)
        if [[ -z "$declared_size" || "$declared_size" -lt "$actual_size" ]]; then
            fail "extractsize ${declared_size:-missing} KiB is smaller than extracted size ${actual_size} KiB"
        fi

        tar -xOf "$output_spk" package.tgz | tar -tzf - | grep -q '^./port_conf/unified-hifi-control.sc$' \
            || fail "SPK must contain its DSM firewall protocol file"
    done

    arm_spk="${test_tmp}/armv8.spk"
    "${SYNOLOGY_DIR}/build-spk.sh" 3.4.1 armv8 "$test_binary" "$arm_spk" \
        || fail "build-spk.sh failed for armv8"
    arm_arch=$(tar -xOf "$arm_spk" INFO | sed -n 's/^arch="\([^"]*\)"$/\1/p')
    [[ "$arm_arch" == 'armv8' ]] || fail "ARM SPK arch is ${arm_arch}, expected armv8"

    if "${SYNOLOGY_DIR}/build-spk.sh" 3.5.0-beta.1 x86_64 "$test_binary" "${test_tmp}/invalid.spk" 2>/dev/null; then
        fail "build-spk.sh must reject prerelease formats without an explicit DSM ordering rule"
    fi
fi

privilege_file="${SYNOLOGY_DIR}/conf/privilege"
python3 -S -m json.tool "$privilege_file" >/dev/null || fail "conf/privilege is invalid JSON"
postuninst_run_as=$(python3 -S -c '
import json, sys
with open(sys.argv[1], encoding="utf-8") as source:
    privilege = json.load(source)
print(next((entry.get("run-as", "") for entry in privilege.get("ctrl-script", []) if entry.get("action") == "postuninst"), ""))
' "$privilege_file")
[[ "$postuninst_run_as" == root ]] || fail "postuninst must run as root so it can remove the package @appdata directory"
python3 -S -m json.tool "${SYNOLOGY_DIR}/conf/resource" >/dev/null || fail "conf/resource is invalid JSON"
assert_contains "${SYNOLOGY_DIR}/conf/resource" '"port-config"' "conf/resource must register the package firewall protocol file"

if ((FAILURES > 0)); then
    echo "Synology package contract failed with ${FAILURES} finding(s)." >&2
    exit 1
fi

echo "Synology package contract passed."
