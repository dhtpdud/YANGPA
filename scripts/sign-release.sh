#!/usr/bin/env bash
# 릴리스의 SHA256SUMS.txt를 배포자 PC의 Ed25519 키로 서명해 같은 폴더에 SHA256SUMS.txt.sig를 만듭니다.
# 앱 업데이트는 이 서명이 맞는 릴리스만 받고 설치합니다(공개 키: crates/yangpa-core/src/release.rs).
# 키는 GitHub·CI에 두지 않습니다(두면 GitHub가 털릴 때 가짜 업데이트를 막지 못함). 저장소 루트에서 돌립니다.
#
#   scripts/sign-release.sh <SHA256SUMS.txt> [키 파일]
#
# 키 파일 기본값: $YANGPA_RELEASE_KEY, 없으면 ~/YANGPA-signing/release-ed25519.pem. 키 만들기(한 번만):
#   openssl genpkey -algorithm ed25519 -out release-ed25519.pem
# 새 키를 만들면 공개 키(아래 확인이 출력하는 hex)를 release.rs에 넣은 판부터 그 키로 서명한 릴리스를 받습니다.
set -euo pipefail

sums="${1:?SHA256SUMS.txt 경로를 주세요}"
key="${2:-${YANGPA_RELEASE_KEY:-$HOME/YANGPA-signing/release-ed25519.pem}}"
[[ -f "$sums" ]] || { echo "목록이 없습니다: $sums" >&2; exit 1; }
[[ -f "$key" ]] || { echo "서명 키가 없습니다: $key" >&2; exit 1; }

want="$(grep -o 'RELEASE_KEY_HEX: &str = "[0-9a-f]*"' crates/yangpa-core/src/release.rs | grep -o '[0-9a-f]\{64\}')"
got="$(openssl pkey -in "$key" -pubout -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n')"
if [[ "$got" != "$want" ]]; then
  echo "이 키의 공개 키($got)가 앱에 든 공개 키($want)와 다릅니다" >&2
  exit 1
fi

openssl pkeyutl -sign -rawin -inkey "$key" -in "$sums" -out "$sums.sig"
pub="$(mktemp)"
trap 'rm -f "$pub"' EXIT
openssl pkey -in "$key" -pubout -out "$pub"
openssl pkeyutl -verify -pubin -inkey "$pub" -rawin -in "$sums" -sigfile "$sums.sig" >/dev/null
echo "서명했습니다: $sums.sig (목록을 고치면 다시 서명하세요)"
