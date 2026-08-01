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
export PATH=$QPKG_ROOT:$PATH

export PIDF=${QPKG_ROOT}/unified-hifi-control.pid
export LOGF=${QPKG_ROOT}/unified-hifi-control.log

# Configuration contains server-side credentials and persistent-operation backups.
# Do not let a permissive NAS-wide umask make newly-created package state readable
# by other local users.
umask 077

case "$1" in
  start)
    ENABLED=$(/sbin/getcfg $QPKG_NAME Enable -u -d FALSE -f $CONF)
    if [ "$ENABLED" != "TRUE" ]; then
        echo "$QPKG_NAME is disabled."
        exit 1
    fi

    cd "$QPKG_ROOT" || { echo "Failed to cd to $QPKG_ROOT"; exit 1; }

    # Start the static binary (musl-linked, no dependencies)
    "${QPKG_ROOT}/unified-hifi-control" >> "$LOGF" 2>&1 &
    echo $! > "$PIDF"

    echo "$QPKG_NAME started."
    ;;

  stop)
    # Kill by PID file first
    if [ -e "$PIDF" ]; then
        PID=$(cat "$PIDF")
        if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
            # Graceful shutdown first, then force if needed
            kill "$PID" 2>/dev/null
            sleep 2
            kill -0 "$PID" 2>/dev/null && kill -9 "$PID" 2>/dev/null
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
    if [ -f "$PIDF" ] && kill -0 "$(cat "$PIDF")" 2>/dev/null; then
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
