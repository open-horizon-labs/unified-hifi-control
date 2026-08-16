# QNAP's official QDK release, wrapped in a repository-owned builder image.
# The base image and downloaded tool are both immutable to keep CI auditable.
FROM ubuntu:20.04@sha256:8feb4d8ca5354def3d8fce243717141ce31e2c428701f6682bd2fafe15388214

ARG QDK_VERSION=2.5.3
ARG QDK_DEB_SHA256=17b3841b7d4590a4ee025844ba583304b5e3c497d9fa8934d5175131d3908022

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install --no-install-recommends --yes \
        ca-certificates curl fakeroot gnupg2 pv rsync xz-utils \
    && curl --fail --silent --show-error --location \
        "https://github.com/qnap-dev/QDK/releases/download/v${QDK_VERSION}/qdk_${QDK_VERSION}_amd64.deb" \
        --output /tmp/qdk.deb \
    && echo "${QDK_DEB_SHA256}  /tmp/qdk.deb" | sha256sum --check --strict \
    && apt-get install --no-install-recommends --yes /tmp/qdk.deb \
    && rm -f /tmp/qdk.deb \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
