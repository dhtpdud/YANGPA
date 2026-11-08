#!/usr/bin/env bash
# Android 빌드 도구를 Debian·Ubuntu에 설치합니다(root로 실행, 다시 돌려도 됩니다). Android SDK 라이선스에 동의합니다.
# SQLCipher의 OpenSSL은 Unix식 make·perl로 빌드해서 Windows에서는 전용 WSL 배포판에서 씁니다(docs/android.md).
set -euo pipefail

RUST_VERSION=1.99.1
NODE_VERSION=22.23.4
CMDLINE_TOOLS=16111833
ANDROID_PLATFORM=36
BUILD_TOOLS=36.1.0
# r28부터 Play가 요구하는 16KB 페이지로 정렬해 링크합니다.
NDK_VERSION=29.0.14206865

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
  ca-certificates curl unzip xz-utils git perl make cmake g++ pkg-config rsync openjdk-21-jdk-headless

curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
  | tar -xJ -C /usr/local --strip-components=1

export ANDROID_HOME=/opt/android-sdk
if [[ ! -x "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" ]]; then
  tmp=$(mktemp -d)
  curl -fsSL -o "$tmp/tools.zip" "https://dl.google.com/android/repository/commandlinetools-linux-${CMDLINE_TOOLS}_latest.zip"
  unzip -q "$tmp/tools.zip" -d "$tmp"
  mkdir -p "$ANDROID_HOME/cmdline-tools"
  mv "$tmp/cmdline-tools" "$ANDROID_HOME/cmdline-tools/latest"
  rm -rf "$tmp"
fi
sdkmanager="$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager"
yes | "$sdkmanager" --licenses >/dev/null || true
"$sdkmanager" --install platform-tools "platforms/android-${ANDROID_PLATFORM}" "build-tools/${BUILD_TOOLS}" "ndk/${NDK_VERSION}"

export RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
if [[ ! -x "$CARGO_HOME/bin/rustup" ]]; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain "$RUST_VERSION"
fi
"$CARGO_HOME/bin/rustup" target add aarch64-linux-android

ndk="$ANDROID_HOME/ndk/$NDK_VERSION"
cat >/etc/profile.d/yangpa-android.sh <<EOF
export ANDROID_HOME=$ANDROID_HOME NDK_HOME=$ndk RUSTUP_HOME=$RUSTUP_HOME CARGO_HOME=$CARGO_HOME
export JAVA_HOME=\$(dirname \$(dirname \$(readlink -f /usr/bin/java)))
export PATH=$CARGO_HOME/bin:$ANDROID_HOME/platform-tools:$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin:\$PATH
# tauri CLI 없이 cargo check를 돌릴 때 쓰는 값(tauri android build는 같은 값을 스스로 정합니다).
export CC_aarch64_linux_android=aarch64-linux-android26-clang CXX_aarch64_linux_android=aarch64-linux-android26-clang++
export AR_aarch64_linux_android=llvm-ar RANLIB_aarch64_linux_android=llvm-ranlib
# 링커 값은 cargo 지문에 들어갑니다. tauri CLI는 절대 경로를 쓰고 Gradle 쪽 cargo는 이 값을 물려받으므로, 글자까지 같지 않으면
# 빌드 한 번에 모든 크레이트를 두 번 다시 컴파일합니다.
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android26-clang
EOF
