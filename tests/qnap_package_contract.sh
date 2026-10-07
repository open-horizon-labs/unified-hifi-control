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
assert_contains "${QNAP_DIR}/shared/install.sh" 'touch .*QPKG_ROOT.*/config/hiphi.env' \
    "QNAP install must create the persistent HiPhi connector environment"
assert_contains "${QNAP_DIR}/shared/install.sh" 'chmod 600 .*QPKG_ROOT.*/config/hiphi.env' \
    "QNAP HiPhi connector environment must be owner-only"
assert_contains "${QNAP_DIR}/shared/unified-hifi-control.sh" 'HIPHI_ENV_FILE=.*UHC_CONFIG_DIR.*/hiphi.env' \
    "QNAP service must load persistent HiPhi connector settings"
assert_contains "${QNAP_DIR}/shared/unified-hifi-control.sh" 'UHC_HIPHI_SESSION_ISSUER_KEYS' \
    "QNAP service must load the dedicated session verifier ring"
assert_contains "${QNAP_DIR}/shared/unified-hifi-control.sh" 'UHC_HIPHI_COMMAND_ISSUER_KEYS' \
    "QNAP service must load the dedicated command verifier ring"
assert_contains "${QNAP_DIR}/shared/install.sh" 'touch .*QPKG_ROOT.*/config/home-assistant.env' \
    "QNAP install must create the standalone Home Assistant environment file"
assert_contains "${QNAP_DIR}/shared/install.sh" 'chmod 600 .*QPKG_ROOT.*/config/home-assistant.env' \
    "QNAP Home Assistant credentials must be owner-only"
assert_contains "${QNAP_DIR}/shared/unified-hifi-control.sh" 'HOME_ASSISTANT_ENV_FILE=.*UHC_CONFIG_DIR.*/home-assistant.env' \
    "QNAP service must load standalone Home Assistant configuration"
assert_contains "${QNAP_DIR}/shared/unified-hifi-control.sh" 'load_home_assistant_config' \
    "QNAP service must validate and load Home Assistant settings"
assert_contains "${QNAP_DIR}/shared/unified-hifi-control.sh" 'UHC_HA_API_URL' \
    "QNAP Home Assistant configuration must allow only the configured API URL"
assert_contains "${QNAP_DIR}/shared/unified-hifi-control.sh" 'UHC_HA_API_TOKEN' \
    "QNAP Home Assistant configuration must allow only the configured API token"
if grep -Eq '(^|[[:space:]])(source|\.)[[:space:]]+.*home-assistant\.env|eval .*home-assistant\.env' \
    "${QNAP_DIR}/shared/unified-hifi-control.sh"; then
    fail "QNAP Home Assistant config must be parsed as data, never sourced or evaluated"
fi

for script in "${QNAP_DIR}/shared"/*.sh; do
    [[ -f "$script" ]] || continue
    sh -n "$script" || fail "$(basename "$script") has invalid POSIX shell syntax"
done

# Exercise the actual data-only loader in isolation.  Its fixture token is
# deliberately synthetic; the real credential must never appear in test output.
tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT
ha_loader=$(sed -n '/^load_home_assistant_config() {$/,/^}$/p' \
    "${QNAP_DIR}/shared/unified-hifi-control.sh")
if [[ -z "$ha_loader" ]]; then
    fail "QNAP service must define its Home Assistant settings loader"
else
    printf '%s\n' 'UHC_HA_API_URL=http://127.0.0.1:8123' 'UHC_HA_API_TOKEN=synthetic-test-token' \
        > "${tmp_dir}/valid.env"
    if ! HOME_ASSISTANT_ENV_FILE="${tmp_dir}/valid.env" bash -c \
        "$ha_loader; load_home_assistant_config && [[ \"\$UHC_HA_API_URL\" == http://127.0.0.1:8123 && \"\$UHC_HA_API_TOKEN\" == synthetic-test-token ]]"; then
        fail "QNAP service must load both allowed Home Assistant settings"
    fi

    : > "${tmp_dir}/empty.env"
    if ! HOME_ASSISTANT_ENV_FILE="${tmp_dir}/empty.env" bash -c \
        "$ha_loader; load_home_assistant_config"; then
        fail "an empty Home Assistant config must leave the optional integration disabled"
    fi

    printf '%s\n' 'UHC_HA_API_URL=http://127.0.0.1:8123' > "${tmp_dir}/partial.env"
    if HOME_ASSISTANT_ENV_FILE="${tmp_dir}/partial.env" bash -c \
        "$ha_loader; load_home_assistant_config" >/dev/null 2>&1; then
        fail "QNAP service must reject a partial Home Assistant credential pair"
    fi

    printf '%s\n' 'UHC_HA_API_URL=http://127.0.0.1:8123' 'UNKNOWN_SETTING=bad' \
        > "${tmp_dir}/unknown.env"
    if HOME_ASSISTANT_ENV_FILE="${tmp_dir}/unknown.env" bash -c \
        "$ha_loader; load_home_assistant_config" >/dev/null 2>&1; then
        fail "QNAP service must reject unrecognized Home Assistant settings"
    fi

    printf '%s\n' 'UHC_HA_API_URL=http://127.0.0.1:8123' \
        'UHC_HA_API_TOKEN=synthetic-test-token' 'touch /tmp/uhc-ha-env-injection' \
        > "${tmp_dir}/injection.env"
    rm -f /tmp/uhc-ha-env-injection
    if HOME_ASSISTANT_ENV_FILE="${tmp_dir}/injection.env" bash -c \
        "$ha_loader; load_home_assistant_config" >/dev/null 2>&1; then
        fail "QNAP service must reject shell-code lines in Home Assistant config"
    fi
    if [[ -e /tmp/uhc-ha-env-injection ]]; then
        fail "QNAP Home Assistant config must never execute shell code"
        rm -f /tmp/uhc-ha-env-injection
    fi
fi

# Keep the x86_64 package tied to the hardened static Linux artifact.  These
# checks catch accidental ARM/host-binary substitutions before QDK packaging.
assert_contains "$WORKFLOW" 'build-qnap-x64:' \
    "workflow must retain a dedicated QNAP x86_64 job"
assert_contains "$WORKFLOW" 'name: binary-x86_64-unknown-linux-musl' \
    "QNAP x86_64 must download the x86_64 musl artifact"
assert_contains "$WORKFLOW" 'cp dist/bin/unified-hifi-linux-x64 qnap-build/shared/unified-hifi-control' \
    "QNAP x86_64 must package the Linux x64 binary"
assert_contains "$WORKFLOW" 'cp dist/bin/uhc-hiphi-pair-x64 qnap-build/shared/uhc-hiphi-pair' \
    "QNAP x86_64 must package the local HiPhi pairing helper"
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
