#!/bin/bash
# Xcode "Build Rust core" phase: builds crates/vc-mobile as a static library for
# the platform being built. The app links it straight from
# target/<triple>/<debug|release>/libvc_mobile.a (LIBRARY_SEARCH_PATHS).
set -euo pipefail

# rustup from Homebrew is keg-only; Xcode does not read the shell profile.
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

case "${PLATFORM_NAME:-iphonesimulator}" in
    iphonesimulator) T=aarch64-apple-ios-sim ;;
    iphoneos) T=aarch64-apple-ios ;;
    *) echo "error: unsupported platform ${PLATFORM_NAME}" >&2; exit 1 ;;
esac
REL=""
if [ "${CONFIGURATION:-Debug}" = "Release" ]; then
    REL="--release"
fi

cd "${PROJECT_DIR:-$(dirname "$0")}/.."
# Xcode's SDKROOT points at the iOS SDK, which breaks host build scripts.
exec env -u SDKROOT IPHONEOS_DEPLOYMENT_TARGET=17.0 cargo build -p vc-mobile --target "$T" $REL
