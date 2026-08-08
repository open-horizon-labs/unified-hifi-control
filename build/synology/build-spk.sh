#!/bin/bash

set -euo pipefail

usage() {
    echo "Usage: $0 <version> <x86_64|armv8> <binary> <output.spk>" >&2
    exit 2
}

normalize_version() {
    local version=$1

    if [[ "$version" =~ ^([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
        printf '%s-10000\n' "${BASH_REMATCH[1]}"
    elif [[ "$version" =~ ^([0-9]+\.[0-9]+\.[0-9]+)-beta(\.([0-9]+))?$ ]]; then
        local beta_number=${BASH_REMATCH[3]:-0}
        if ((beta_number > 999)); then
            echo "Beta number is too large for Synology version encoding: $version" >&2
            return 1
        fi
        printf '%s-%04d\n' "${BASH_REMATCH[1]}" "$((8000 + beta_number))"
    elif [[ "$version" =~ ^([0-9]+\.[0-9]+\.[0-9]+)-rc\.([0-9]+)$ ]]; then
        local rc_number=${BASH_REMATCH[2]}
        if ((rc_number > 999)); then
            echo "Release-candidate number is too large for Synology version encoding: $version" >&2
            return 1
        fi
        printf '%s-%04d\n' "${BASH_REMATCH[1]}" "$((9000 + rc_number))"
    elif [[ "$version" =~ ^0\.0\.0-pr([0-9]+)$ ]]; then
        printf '0.0.0-%04d\n' "${BASH_REMATCH[1]}"
    elif [[ "$version" == "0.0.0-dev" ]]; then
        printf '0.0.0-0001\n'
    else
        echo "Unsupported Synology package version: $version" >&2
        return 1
    fi
}

[[ $# -eq 4 ]] || usage

SOURCE_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SOURCE_VERSION=$1
ARCH=$2
BINARY=$3
OUTPUT=$4

case "$ARCH" in
    x86_64 | armv8) ;;
    *)
        echo "Unsupported Synology architecture family: $ARCH" >&2
        exit 2
        ;;
esac

[[ -f "$BINARY" ]] || {
    echo "Binary not found: $BINARY" >&2
    exit 2
}

DSM_VERSION=$(normalize_version "$SOURCE_VERSION")
OUTPUT_DIR=$(dirname "$OUTPUT")
mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR=$(cd "$OUTPUT_DIR" && pwd)
OUTPUT="${OUTPUT_DIR}/$(basename "$OUTPUT")"

STAGE_DIR=$(mktemp -d "${TMPDIR:-/tmp}/uhc-synology-build.XXXXXX")
trap 'rm -rf "$STAGE_DIR"' EXIT

mkdir -p "${STAGE_DIR}/package"
cp -R "${SOURCE_DIR}/scripts" "${SOURCE_DIR}/conf" "${STAGE_DIR}/"
cp -R "${SOURCE_DIR}/package/." "${STAGE_DIR}/package/"
cp "${SOURCE_DIR}/PACKAGE_ICON.PNG" "${SOURCE_DIR}/PACKAGE_ICON_256.PNG" "${STAGE_DIR}/"
cp "$BINARY" "${STAGE_DIR}/package/unified-hifi-control"
chmod +x "${STAGE_DIR}/package/unified-hifi-control" "${STAGE_DIR}"/scripts/*

sed \
    -e "s/{{VERSION}}/${DSM_VERSION}/g" \
    -e "s/{{ARCH}}/${ARCH}/g" \
    "${SOURCE_DIR}/INFO" > "${STAGE_DIR}/INFO"

tar -czf "${STAGE_DIR}/package.tgz" -C "${STAGE_DIR}/package" .
EXTRACT_SIZE=$(du -sk "${STAGE_DIR}/package" | cut -f1)
printf 'extractsize="%s"\n' "$EXTRACT_SIZE" >> "${STAGE_DIR}/INFO"

tar -cf "$OUTPUT" -C "$STAGE_DIR" \
    INFO PACKAGE_ICON.PNG PACKAGE_ICON_256.PNG package.tgz scripts conf

echo "Built ${OUTPUT} (INFO version ${DSM_VERSION}, arch ${ARCH})"
