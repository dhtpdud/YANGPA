#!/usr/bin/env bash
# Android 앱을 빌드합니다. Linux(Windows는 전용 WSL 배포판, docs/android.md)에서 저장소 루트를 기준으로 돌립니다.
#
#   scripts/build-android.sh           시험용 APK(디버그)
#   scripts/build-android.sh --release Play에 올릴 AAB(서명: gen/android/key.properties)
set -euo pipefail

cd "$(dirname "$0")/.."
jni=apps/desktop/src-tauri/gen/android/app/src/main/jniLibs/arm64-v8a
# AGPL과 함께 넣은 프로그램의 라이선스 사본을 앱에 함께 넣습니다(decisions.md "배포물").
licenses=apps/desktop/src-tauri/gen/android/app/src/main/assets/licenses
mkdir -p "$jni" "$licenses"
cp LICENSE NOTICE.md LICENSE-SUPPORTER.md TRADEMARK.md "$licenses/"

read -r name sum url inner < <(grep -v '^#' scripts/lyrebird-android-files.txt)
if ! echo "$sum  $jni/$name" | sha256sum -c --quiet 2>/dev/null || [[ ! -f "$licenses/lyrebird.txt" ]]; then
  tmp=$(mktemp -d)
  curl -fsSL -o "$tmp/bundle.tar.gz" "$url"
  tar -xzf "$tmp/bundle.tar.gz" -C "$tmp" "$inner" docs/lyrebird.txt
  cp "$tmp/$inner" "$jni/$name"
  cp "$tmp/docs/lyrebird.txt" "$licenses/lyrebird.txt"
  rm -rf "$tmp"
  echo "$sum  $jni/$name" | sha256sum -c --quiet
fi

cd apps/desktop
npm ci --no-audit --no-fund
if [[ "${1:-}" == "--release" ]]; then
  npx tauri android build --aab --target aarch64
else
  npx tauri android build --debug --apk --target aarch64
fi
