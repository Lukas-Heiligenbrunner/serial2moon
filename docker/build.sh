#!/usr/bin/env bash
# Cross-compile serial2moon for the requested docker platform. Runs in the (native,
# amd64) builder stage of the Dockerfile, so cargo runs at full speed and just emits a
# binary for the target triple. No C dependencies => only a cross-linker is needed.
set -euxo pipefail

TARGET_PLATFORM="${1:?usage: build.sh <docker-platform>}"

case "$TARGET_PLATFORM" in
    linux/amd64)
        TRIPLE=x86_64-unknown-linux-gnu
        ;;
    linux/arm64 | linux/arm64/v8)
        TRIPLE=aarch64-unknown-linux-gnu
        # gcc-* alone omits the target C runtime (crt*.o, libc) — needed to link.
        PKG="gcc-aarch64-linux-gnu libc6-dev-arm64-cross"
        LINKER=aarch64-linux-gnu-gcc
        ;;
    linux/arm/v7)
        TRIPLE=armv7-unknown-linux-gnueabihf
        PKG="gcc-arm-linux-gnueabihf libc6-dev-armhf-cross"
        LINKER=arm-linux-gnueabihf-gcc
        ;;
    *)
        echo "unsupported platform: $TARGET_PLATFORM" >&2
        exit 1
        ;;
esac

rustup target add "$TRIPLE"

if [ -n "${PKG:-}" ]; then
    apt-get update
    # shellcheck disable=SC2086 # word-splitting is intended: $PKG is a package list
    apt-get install -y --no-install-recommends $PKG
    rm -rf /var/lib/apt/lists/*
fi

if [ -n "${LINKER:-}" ]; then
    VARNAME="CARGO_TARGET_$(echo "$TRIPLE" | tr '[:lower:]-' '[:upper:]_')_LINKER"
    export "$VARNAME=$LINKER"
fi

cargo build --release --locked --target "$TRIPLE"
cp "target/$TRIPLE/release/serial2moon" /serial2moon
