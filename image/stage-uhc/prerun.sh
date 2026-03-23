#!/bin/bash -e
# pi-gen prerun script for stage-uhc
# This runs before any sub-stages to validate prerequisites
if [ ! -f "${STAGE_DIR}/01-uhc-binary/files/unified-hifi-control" ]; then
    echo "ERROR: UHC binary not found. Run build.sh, not pi-gen directly." >&2
    exit 1
fi
