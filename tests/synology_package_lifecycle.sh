#!/bin/bash

set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
IMAGE=${SYNOLOGY_TEST_IMAGE:-ubuntu:24.04}
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/uhc-synology-lifecycle.XXXXXX")
trap 'rm -rf "$TEST_ROOT"' EXIT

mkdir -p "${TEST_ROOT}/target" "${TEST_ROOT}/scripts" "${TEST_ROOT}/var" "${TEST_ROOT}/home"
cp "${ROOT_DIR}"/build/synology/scripts/* "${TEST_ROOT}/scripts/"

# DSM exposes persistent package data through /var/packages/<name>/var, which
# points to a volume's @appdata directory.
rmdir "${TEST_ROOT}/var"
ln -s /volume2/@appdata/unified-hifi-control "${TEST_ROOT}/var"

cat > "${TEST_ROOT}/target/unified-hifi-control" <<'DAEMON'
#!/bin/sh

printf '%s\n' "${UHC_CONFIG_DIR:-}" > "${PACKAGE_TEST_ENV_FILE:?}"
printf '%s\n' "${UHC_DATA_DIR:-}" >> "$PACKAGE_TEST_ENV_FILE"
trap 'exit 0' TERM INT
while :; do
    sleep 1
done
DAEMON
chmod +x "${TEST_ROOT}/target/unified-hifi-control" "${TEST_ROOT}/scripts/start-stop-status"

docker run --rm \
    --platform "${SYNOLOGY_TEST_PLATFORM:-linux/amd64}" \
    --env "TEST_HOST_UID=$(id -u)" \
    --env "TEST_HOST_GID=$(id -g)" \
    --mount "type=bind,source=${TEST_ROOT},target=/var/packages/unified-hifi-control" \
    "$IMAGE" \
    bash -euo pipefail -c '
        useradd --system --home-dir /var/packages/unified-hifi-control/home unified-hifi-control
        mkdir -p /volume2/@appdata/unified-hifi-control/firmware
        chown -R unified-hifi-control:unified-hifi-control /var/packages/unified-hifi-control
        chown -R unified-hifi-control:unified-hifi-control /volume2/@appdata/unified-hifi-control

        run_as_package() {
            su -s /bin/sh unified-hifi-control -c "$1"
        }

        control=/var/packages/unified-hifi-control/scripts/start-stop-status
        export PACKAGE_TEST_ENV_FILE=/var/packages/unified-hifi-control/var/environment

        # Exercise install and upgrade hook order before testing the service.
        run_as_package "/var/packages/unified-hifi-control/scripts/preinst"
        run_as_package "/var/packages/unified-hifi-control/scripts/postinst"
        run_as_package "/var/packages/unified-hifi-control/scripts/preupgrade"
        run_as_package "/var/packages/unified-hifi-control/scripts/preuninst"
        printf "preserve during upgrade\n" > /var/packages/unified-hifi-control/var/firmware/version.json
        SYNOPKG_PKG_STATUS=UPGRADE \
            SYNOPKG_PKGDEST_VOL=/volume2 \
            SYNOPKG_PKGVAR=/var/packages/unified-hifi-control/var \
            /var/packages/unified-hifi-control/scripts/postuninst
        test -f /volume2/@appdata/unified-hifi-control/firmware/version.json
        run_as_package "/var/packages/unified-hifi-control/scripts/preinst"
        run_as_package "/var/packages/unified-hifi-control/scripts/postinst"
        run_as_package "/var/packages/unified-hifi-control/scripts/postupgrade"

        run_as_package "$control start"
        run_as_package "$control start"
        run_as_package "$control status"

        test "$(sed -n 1p "$PACKAGE_TEST_ENV_FILE")" = /var/packages/unified-hifi-control/var
        test "$(sed -n 2p "$PACKAGE_TEST_ENV_FILE")" = /var/packages/unified-hifi-control/var

        run_as_package "$control stop"

        set +e
        run_as_package "$control status"
        status_rc=$?
        set -e
        test "$status_rc" -eq 3

        # A stale PID file has DSM status 1, distinct from a normal stop.
        printf "999999\n" > /var/packages/unified-hifi-control/var/unified-hifi-control.pid
        set +e
        run_as_package "$control status"
        stale_status_rc=$?
        set -e
        test "$stale_status_rc" -eq 1
        rm /var/packages/unified-hifi-control/var/unified-hifi-control.pid

        run_as_package "/var/packages/unified-hifi-control/scripts/preuninst"

        # A lookalike @appdata path outside the DSM-selected package volume must
        # never be removed by the root-running uninstall hook.
        mkdir -p /tmp/@appdata/unified-hifi-control
        printf "preserve unexpected path\n" > /tmp/@appdata/unified-hifi-control/sentinel
        set +e
        SYNOPKG_PKG_STATUS=UNINSTALL \
            SYNOPKG_PKGDEST_VOL=/volume2 \
            SYNOPKG_PKGVAR=/tmp/@appdata/unified-hifi-control \
            /var/packages/unified-hifi-control/scripts/postuninst
        unexpected_path_rc=$?
        set -e
        test "$unexpected_path_rc" -ne 0
        test -f /tmp/@appdata/unified-hifi-control/sentinel

        SYNOPKG_PKG_STATUS=UNINSTALL \
            SYNOPKG_PKGDEST_VOL=/volume2 \
            SYNOPKG_PKGVAR=/var/packages/unified-hifi-control/var \
            /var/packages/unified-hifi-control/scripts/postuninst
        test ! -e /volume2/@appdata/unified-hifi-control

        # Restore bind-mount ownership so the host-side cleanup trap works on
        # Linux runners where container UID ownership is preserved.
        chown -R "$TEST_HOST_UID:$TEST_HOST_GID" /var/packages/unified-hifi-control
    '

echo "Synology unprivileged lifecycle test passed."
