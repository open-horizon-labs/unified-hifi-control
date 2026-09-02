#!/bin/sh
CONF=/etc/config/qpkg.conf
QPKG_NAME="unified-hifi-control"
QPKG_ROOT=`/sbin/getcfg $QPKG_NAME Install_Path -f ${CONF}`

export QPKG_ROOT
export QPKG_NAME
export SHELL=/bin/sh
export LC_ALL=en_US.UTF-8
export USER=admin
export LANG=en_US.UTF-8
export LC_CTYPE=en_US.UTF-8
export HOME=$QPKG_ROOT
# Persist OAuth credentials below the QPKG so upgrades/restarts retain the
# connection without requiring shell configuration.  Operators can override
# this with an environment supplied by their service manager.
export UHC_CONFIG_DIR=${UHC_CONFIG_DIR:-${QPKG_ROOT}/config}
export PATH=$QPKG_ROOT:$PATH
HIPHI_ENV_FILE=${UHC_CONFIG_DIR}/hiphi.env

export PIDF=${QPKG_ROOT}/unified-hifi-control.pid
export UHC_LOG_DIR=${UHC_LOG_DIR:-${QPKG_ROOT}/logs}
export LAUNCH_LOG=${QPKG_ROOT}/unified-hifi-control-launcher.log

# Configuration contains server-side credentials and persistent-operation backups.
# Do not let a permissive NAS-wide umask make newly-created package state readable
# by other local users.
umask 077

# Load data, never shell code.  The pairing helper persists a compact file of
# exact connector keys; parsing a fixed allowlist here means a damaged config
# cannot turn a privileged NAS service restart into arbitrary shell execution.
load_hiphi_config() {
    if [ -L "$HIPHI_ENV_FILE" ]; then
        echo "Refusing unsafe HiPhi connector configuration: $HIPHI_ENV_FILE"
        return 1
    fi
    [ -e "$HIPHI_ENV_FILE" ] || return 0
    if [ ! -f "$HIPHI_ENV_FILE" ]; then
        echo "Refusing unsafe HiPhi connector configuration: $HIPHI_ENV_FILE"
        return 1
    fi

    SEEN_RELAY=false
    SEEN_INSTALLATION=false
    SEEN_SESSION=false
    SEEN_COMMAND=false
    while IFS= read -r HIPHI_LINE || [ -n "$HIPHI_LINE" ]; do
        case "$HIPHI_LINE" in
            ''|'#'*) continue ;;
            *=*) ;;
            *) echo "Invalid HiPhi connector configuration line"; return 1 ;;
        esac
        HIPHI_NAME=${HIPHI_LINE%%=*}
        HIPHI_VALUE=${HIPHI_LINE#*=}
        [ -n "$HIPHI_VALUE" ] || { echo "Empty HiPhi connector setting: $HIPHI_NAME"; return 1; }
        case "$HIPHI_NAME" in
            UHC_HIPHI_RELAY_URL)
                [ "$SEEN_RELAY" = false ] || return 1
                SEEN_RELAY=true
                export UHC_HIPHI_RELAY_URL="$HIPHI_VALUE"
                ;;
            UHC_HIPHI_INSTALLATION_ID)
                [ "$SEEN_INSTALLATION" = false ] || return 1
                SEEN_INSTALLATION=true
                export UHC_HIPHI_INSTALLATION_ID="$HIPHI_VALUE"
                ;;
            UHC_HIPHI_SESSION_ISSUER_KEYS)
                [ "$SEEN_SESSION" = false ] || return 1
                SEEN_SESSION=true
                export UHC_HIPHI_SESSION_ISSUER_KEYS="$HIPHI_VALUE"
                ;;
            UHC_HIPHI_COMMAND_ISSUER_KEYS)
                [ "$SEEN_COMMAND" = false ] || return 1
                SEEN_COMMAND=true
                export UHC_HIPHI_COMMAND_ISSUER_KEYS="$HIPHI_VALUE"
                ;;
            *) echo "Unknown HiPhi connector setting: $HIPHI_NAME"; return 1 ;;
        esac
    done < "$HIPHI_ENV_FILE"
}

# A PID file is only a hint: after a crash or reboot its numeric PID can be reused by an
# unrelated process. Refuse to signal unless /proc still identifies the package binary.
is_our_pid() {
    PID_TO_CHECK=$1
    case "$PID_TO_CHECK" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ "$PID_TO_CHECK" -gt 1 ] 2>/dev/null || return 1
    kill -0 "$PID_TO_CHECK" 2>/dev/null || return 1
    EXPECTED_EXE=$(readlink -f "${QPKG_ROOT}/unified-hifi-control" 2>/dev/null) || return 1
    ACTUAL_EXE=$(readlink -f "/proc/${PID_TO_CHECK}/exe" 2>/dev/null) || return 1
    [ -n "$EXPECTED_EXE" ] && [ "$ACTUAL_EXE" = "$EXPECTED_EXE" ]
}

case "$1" in
  start)
    ENABLED=$(/sbin/getcfg $QPKG_NAME Enable -u -d FALSE -f $CONF)
    if [ "$ENABLED" != "TRUE" ]; then
        echo "$QPKG_NAME is disabled."
        exit 1
    fi

    cd "$QPKG_ROOT" || { echo "Failed to cd to $QPKG_ROOT"; exit 1; }

    load_hiphi_config || { echo "Failed to load HiPhi connector configuration"; exit 1; }

    # UHC owns daily rotation inside UHC_LOG_DIR. This small launcher file is
    # truncated on each start and captures only failures before logging starts.
    "${QPKG_ROOT}/unified-hifi-control" > "$LAUNCH_LOG" 2>&1 &
    echo $! > "$PIDF"

    echo "$QPKG_NAME started."
    ;;

  stop)
    # Kill by PID file first
    if [ -e "$PIDF" ]; then
        PID=$(cat "$PIDF")
        if is_our_pid "$PID"; then
            # Graceful shutdown first, then force if needed
            kill "$PID" 2>/dev/null
            sleep 2
            # Re-check identity: the original process may have exited and the PID may already
            # have been reused during the grace period.
            is_our_pid "$PID" && kill -9 "$PID" 2>/dev/null
        fi
        rm -f "$PIDF"
    fi

    echo "$QPKG_NAME stopped."
    ;;

  restart)
    $0 stop
    $0 start
    ;;

  status)
    if [ -f "$PIDF" ] && is_our_pid "$(cat "$PIDF")"; then
        echo "$QPKG_NAME is running."
        exit 0
    else
        echo "$QPKG_NAME is stopped."
        exit 1
    fi
    ;;

  *)
    echo "Usage: $0 {start|stop|restart|status}"
    exit 1
esac

exit 0
