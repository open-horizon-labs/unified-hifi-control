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
export LOGF=${QPKG_ROOT}/unified-hifi-control.log
export LOG_ROTATOR_PIDF=${QPKG_ROOT}/unified-hifi-control-log-rotator.pid

# Appliance defaults: keep the live log plus three bounded archives (100 MiB
# total at the defaults), and check once a minute. Operators may tighten these
# values without modifying the package script.
LOG_MAX_BYTES=${UHC_LOG_MAX_BYTES:-26214400}
LOG_ARCHIVES=${UHC_LOG_ARCHIVES:-3}
LOG_CHECK_SECONDS=${UHC_LOG_CHECK_SECONDS:-60}
export RUST_LOG=${RUST_LOG:-unified_hifi_control=info,tower_http=info,roon_api=info}

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

is_positive_integer() {
    case "$1" in
        ''|*[!0-9]*|0) return 1 ;;
        *) return 0 ;;
    esac
}

validate_log_policy() {
    is_positive_integer "$LOG_MAX_BYTES" || { echo "Invalid UHC_LOG_MAX_BYTES"; return 1; }
    is_positive_integer "$LOG_ARCHIVES" || { echo "Invalid UHC_LOG_ARCHIVES"; return 1; }
    is_positive_integer "$LOG_CHECK_SECONDS" || { echo "Invalid UHC_LOG_CHECK_SECONDS"; return 1; }
    [ "$LOG_ARCHIVES" -le 9 ] || { echo "UHC_LOG_ARCHIVES must be between 1 and 9"; return 1; }
}

rotate_log_if_needed() {
    [ -f "$LOGF" ] || { : > "$LOGF" || return 1; }
    LOG_BYTES=$(wc -c < "$LOGF" 2>/dev/null | tr -d ' ')
    case "$LOG_BYTES" in ''|*[!0-9]*) return 1 ;; esac
    [ "$LOG_BYTES" -gt "$LOG_MAX_BYTES" ] || return 0

    # Preserve only a bounded tail. Copying a multi-gigabyte runaway log would
    # consume the remaining volume before rotation could reclaim it.
    ROTATE_TMP=${LOGF}.rotate.$$
    tail -c "$LOG_MAX_BYTES" "$LOGF" > "$ROTATE_TMP" || {
        rm -f "$ROTATE_TMP"
        return 1
    }
    chmod 600 "$ROTATE_TMP"

    ARCHIVE=$LOG_ARCHIVES
    while [ "$ARCHIVE" -gt 1 ]; do
        PREVIOUS=$((ARCHIVE - 1))
        [ ! -e "${LOGF}.${PREVIOUS}" ] || mv -f "${LOGF}.${PREVIOUS}" "${LOGF}.${ARCHIVE}"
        ARCHIVE=$PREVIOUS
    done
    mv -f "$ROTATE_TMP" "${LOGF}.1"
    : > "$LOGF"
    chmod 600 "$LOGF"
}

is_our_rotator_pid() {
    ROTATOR_PID_TO_CHECK=$1
    EXPECTED_SERVICE_PID=$2
    case "$ROTATOR_PID_TO_CHECK:$EXPECTED_SERVICE_PID" in
        *[!0-9:]*) return 1 ;;
    esac
    [ "$ROTATOR_PID_TO_CHECK" -gt 1 ] 2>/dev/null || return 1
    kill -0 "$ROTATOR_PID_TO_CHECK" 2>/dev/null || return 1
    ROTATOR_COMMAND=$(tr '\000' ' ' < "/proc/${ROTATOR_PID_TO_CHECK}/cmdline" 2>/dev/null) || return 1
    case "$ROTATOR_COMMAND" in
        *"${QPKG_ROOT}/unified-hifi-control.sh rotate ${EXPECTED_SERVICE_PID}"*) return 0 ;;
        *) return 1 ;;
    esac
}

case "$1" in
  rotate)
    SERVICE_PID=$2
    validate_log_policy || exit 1
    while is_our_pid "$SERVICE_PID"; do
        sleep "$LOG_CHECK_SECONDS"
        is_our_pid "$SERVICE_PID" || break
        rotate_log_if_needed || echo "Unified Hi-Fi Control log rotation failed" >&2
    done
    ;;

  start)
    ENABLED=$(/sbin/getcfg $QPKG_NAME Enable -u -d FALSE -f $CONF)
    if [ "$ENABLED" != "TRUE" ]; then
        echo "$QPKG_NAME is disabled."
        exit 1
    fi

    cd "$QPKG_ROOT" || { echo "Failed to cd to $QPKG_ROOT"; exit 1; }

    load_hiphi_config || { echo "Failed to load HiPhi connector configuration"; exit 1; }
    validate_log_policy || exit 1
    rotate_log_if_needed || { echo "Failed to apply log retention policy"; exit 1; }

    # Start the static binary (musl-linked, no dependencies)
    "${QPKG_ROOT}/unified-hifi-control" >> "$LOGF" 2>&1 &
    SERVICE_PID=$!
    echo "$SERVICE_PID" > "$PIDF"
    "${QPKG_ROOT}/unified-hifi-control.sh" rotate "$SERVICE_PID" >> "$LOGF" 2>&1 &
    echo $! > "$LOG_ROTATOR_PIDF"

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
        if [ -e "$LOG_ROTATOR_PIDF" ]; then
            ROTATOR_PID=$(cat "$LOG_ROTATOR_PIDF")
            if is_our_rotator_pid "$ROTATOR_PID" "$PID"; then
                kill "$ROTATOR_PID" 2>/dev/null
            fi
            rm -f "$LOG_ROTATOR_PIDF"
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
