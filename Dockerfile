# syntax=docker/dockerfile:1
#
# 양파 메신저(가칭) 헤드리스 시더 이미지: 코어 데몬(yangpad)과 CLI(yangpa)만 담습니다(GUI 없음).
#
#   docker build -t yangpa-seeder .          # 저장소 루트에서
#   docker compose -f deploy/docker-compose.yml up -d
#
# 설정·운영은 docs/headless.md를 보세요. 데이터(DB·DB 키·블롭·소유자 토큰)는 /var/lib/yangpa 볼륨에만 씁니다.

ARG RUST_VERSION=1.99.1
ARG DEBIAN_SUITE=trixie

# ───────────── 빌드 ─────────────
FROM rust:${RUST_VERSION}-slim-${DEBIAN_SUITE} AS build

# SQLCipher가 쓰는 OpenSSL을 소스에서 정적으로 빌드하므로 perl·make가 필요합니다.
# cmake는 aws-lc가 기본 빌드 방식을 쓸 수 없을 때의 대비입니다. 빌드 단계 도구일 뿐이라 판을 고정하지 않습니다
# (데비안 포인트 릴리스마다 옛 판이 저장소에서 빠져 빌드가 깨짐). 실행 이미지에는 들어가지 않습니다.
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends perl make cmake g++ pkg-config \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# 데몬과 CLI만 빌드합니다(데스크톱 앱·MCP 서버는 시더에 필요 없음). Cargo.lock 그대로(--locked).
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p yangpa-daemon -p yangpa-cli --bin yangpad --bin yangpa \
 && install -D -m 0755 target/release/yangpad /out/yangpad \
 && install -D -m 0755 target/release/yangpa /out/yangpa

# ───────────── 실행 ─────────────
FROM debian:${DEBIAN_SUITE}-slim

ARG VERSION=dev
LABEL org.opencontainers.image.title="yangpa-seeder" \
      org.opencontainers.image.description="양파 메신저(가칭) 헤드리스 시더: yangpad + yangpa CLI" \
      org.opencontainers.image.version="${VERSION}"

# 고정 UID/GID(10001)의 비루트 사용자. 바인드 마운트를 쓰면 호스트 디렉터리를 이 번호로 chown 하세요.
# 데이터 경로는 0700: 데몬은 이미 있는 디렉터리의 권한을 바꾸지 않고, Arti는 남이 쓸 수 있는 경로를 거부합니다.
RUN groupadd --system --gid 10001 yangpa \
 && useradd --system --uid 10001 --gid yangpa --home-dir /var/lib/yangpa --no-create-home \
            --shell /usr/sbin/nologin yangpa \
 && install -d -m 0700 -o yangpa -g yangpa /var/lib/yangpa

COPY --from=build /out/ /usr/local/bin/

# CLI(`docker exec … yangpa …`)와 데몬이 같은 데이터 경로·소켓을 보게 합니다. 소켓(0600)은 볼륨 안에 두어
# 읽기 전용 루트 파일 시스템에서도 열립니다. 상시 시더라 휴면(구독 끊기)을 끕니다.
# 큰 통화의 SFU 호스트를 먼저 맡고(참가하지 않은 통화도), 미디어는 UDP 47811로 받습니다. 브리지 네트워크에서는
# 컨테이너 IP만 알므로 YANGPA_SFU_ANNOUNCE에 공인 IP를 넣으세요(docs/headless.md "통화 SFU 호스트").
# 데몬은 기본이 Tor지만 시더는 포트 47810을 열고 받는 서버라 직접 연결로 띄웁니다(Tor로 띄우려면 YANGPA_DIRECT=0).
ENV YANGPA_DATA_DIR=/var/lib/yangpa \
    YANGPA_ENDPOINT=/var/lib/yangpa/yangpad.sock \
    YANGPA_DIRECT=1 \
    YANGPA_DORMANT_AFTER=0 \
    YANGPA_SFU_HOST=prefer \
    YANGPA_SFU_PORT=47811

USER 10001:10001
WORKDIR /var/lib/yangpa
VOLUME ["/var/lib/yangpa"]

# 다른 노드가 들어올 고정 포트(TCP·QUIC)와 통화 SFU 미디어(UDP). Tor로 띄우면(YANGPA_DIRECT=0) 쓰지 않습니다(onion 서비스는
# 나가는 연결만 쓰고, Tor로 도는 노드는 SFU를 맡지 않음).
EXPOSE 47810/tcp 47810/udp 47811/udp

# 인증이 필요 없는 system.version으로 로컬 API가 답하는지만 봅니다. 토큰으로 인증하는 명령은 호출마다 감사 로그에
# 남으므로 상태 확인에 쓰지 않습니다(`yangpa version`은 데몬이 없어도 0으로 끝나 JSON에서 데몬 판을 찾음).
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
  CMD ["sh", "-c", "yangpa --json --timeout 5s version | grep -q '\"protocol\"'"]

# DB 키는 데이터 경로의 0600 파일(컨테이너에는 OS 키 저장소가 없음). 디스크 암호화로 볼륨을 보호하세요.
ENTRYPOINT ["yangpad", "--key-storage", "file"]
# 같은 네트워크 탐색(mDNS)은 끄고 고정 포트로 받습니다. 바꿀 때는 이 인자 전체를 다시 적습니다(docs/headless.md).
CMD ["--no-mdns", "--listen", "/ip4/0.0.0.0/tcp/47810", "--listen", "/ip4/0.0.0.0/udp/47810/quic-v1"]
