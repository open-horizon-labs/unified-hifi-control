#!/bin/bash

# Contract checks for the QNAP x86_64 package path.  The package is assembled
# from the Linux musl artifact, so this test deliberately validates the
# workflow contract rather than invoking QDK (which is only available in the
# builder image used by CI).

set -uo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
QNAP_DIR="${ROOT_DIR}/build/qnap"
WORKFLOW="${ROOT_DIR}/.github/workflows/build.yml"
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

assert_contains "${QNAP_DIR}/qpkg.cfg" '^QPKG_NAME="unified-hifi-control"$' \
    "QNAP metadata must retain the stable package name"
assert_contains "${QNAP_DIR}/qpkg.cfg" '^QDK_DATA_DIR_SHARED="shared"$' \
    "QNAP metadata must use the shared QDK2 payload directory"
assert_contains "${QNAP_DIR}/qpkg.cfg" '^QPKG_SERVICE_PROGRAM="unified-hifi-control.sh"$' \
    "QNAP metadata must register the service wrapper"

# A fresh package must provide a private, stable config root for the server's
# encrypted provider credential store.  The wrapper may still be overridden
# by an operator-managed secret volume, but the default cannot depend on an
# interactive shell environment.
assert_contains "${QNAP_DIR}/shared/install.sh" 'mkdir -p .*QPKG_ROOT.*/config' \
    "QNAP install must create the package-owned config directory"
assert_contains "${QNAP_DIR}/shared/install.sh" 'chmod 700 .*QPKG_ROOT.*/config' \
    "QNAP config directory must be owner-only"
assert_contains "${QNAP_DIR}/shared/unified-hifi-control.sh" 'UHC_CONFIG_DIR=.*QPKG_ROOT.*/config' \
    "QNAP service must default UHC_CONFIG_DIR to the package config directory"

for script in "${QNAP_DIR}/shared"/*.sh; do
    [[ -f "$script" ]] || continue
    sh -n "$script" || fail "$(basename "$script") has invalid POSIX shell syntax"
done

# Keep the x86_64 package tied to the hardened static Linux artifact.  These
# checks catch accidental ARM/host-binary substitutions before QDK packaging.
assert_contains "$WORKFLOW" 'build-qnap-x64:' \
    "workflow must retain a dedicated QNAP x86_64 job"
assert_contains "$WORKFLOW" 'name: binary-x86_64-unknown-linux-musl' \
    "QNAP x86_64 must download the x86_64 musl artifact"
assert_contains "$WORKFLOW" 'cp dist/bin/unified-hifi-linux-x64 qnap-build/shared/unified-hifi-control' \
    "QNAP x86_64 must package the Linux x64 binary"
assert_contains "$WORKFLOW" 'docker run --rm --platform linux/amd64' \
    "QDK must run with an explicit amd64 builder platform"
assert_contains "$WORKFLOW" 'unified-hifi-control_\$\{\{ needs\.plan\.outputs\.version \}\}_x86_64\.qpkg' \
    "QNAP x86_64 artifact must carry an x86_64 suffix"
assert_contains "$WORKFLOW" 'name: qnap-x86_64' \
    "QNAP x86_64 artifact must have a stable upload name"

if ((FAILURES > 0)); then
    echo "QNAP x86_64 package contract failed with ${FAILURES} finding(s)." >&2
    exit 1
fi

echo "QNAP x86_64 package contract passed."
