#!/bin/sh
set -eu
# Build the Rust/Bionic executable with the pinned NDK toolchain. This avoids
# cargo-ndk and never downloads, embeds, or copies vendor/runtime bytes.
ndk="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
: "${ndk:?ANDROID_NDK_HOME is required}"
api="${ANDROID_API_LEVEL:-25}"
out="${UE_NAA_NATIVE_LIBS:-${UE_NATIVE_BUILD_ROOT:-/tmp/hiphi-naa-native}/jniLibs}"
case "$(uname -s)" in
  Darwin) host=darwin-x86_64 ;; # NDK uses this directory on Apple Silicon too.
  Linux) host=linux-x86_64 ;;
  *) echo 'Unsupported NDK build host' >&2; exit 2 ;;
esac
tool="$ndk/toolchains/llvm/prebuilt/$host/bin"
[ -x "$tool/aarch64-linux-android${api}-clang" ] || { echo "missing pinned NDK clang: $tool" >&2; exit 2; }
[ -x "$tool/armv7a-linux-androideabi${api}-clang" ] || { echo "missing pinned NDK clang: $tool" >&2; exit 2; }
root="$(CDPATH= cd -- "$(dirname "$0")" && pwd)"
target_dir="${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/hiphi-naa-cargo-target}"
mkdir -p "$out/arm64-v8a" "$out/armeabi-v7a"
(cd "$root" && CARGO_TARGET_DIR="$target_dir" CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$tool/aarch64-linux-android${api}-clang" cargo build --release --target aarch64-linux-android --bin naa-android)
(cd "$root" && CARGO_TARGET_DIR="$target_dir" CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER="$tool/armv7a-linux-androideabi${api}-clang" cargo build --release --target armv7-linux-androideabi --bin naa-android)
install -m 755 "$target_dir/aarch64-linux-android/release/naa-android" "$out/arm64-v8a/libnaa_android.so"
install -m 755 "$target_dir/armv7-linux-androideabi/release/naa-android" "$out/armeabi-v7a/libnaa_android.so"
