#!/usr/bin/env bash
# 화면공유 인코더용 최소 FFmpeg(Windows x64, LGPL 2.1+, 공유 DLL)를 Linux에서 교차 빌드합니다.
# 앱(apps/desktop/src-tauri/src/screen/args.rs)이 쓰는 것만 넣습니다: gfxcapture 캡처(D3D11), fps·hwdownload·format·scale
# 필터, color 소스와 lavfi 입력(확인용), libaom·SVT-AV1·NVENC(AV1·H.264·HEVC) 인코더, FLV·null 출력, pipe 프로토콜.
# 인자를 바꾸면 아래 FF_CONFIGURE와 REQUIRED 목록도 같이 고칩니다. GPL·nonfree 부품은 넣지 않습니다.
#
#   scripts/build-ffmpeg.sh <출력 폴더>     → <출력 폴더>/ffmpeg/ (ffmpeg.exe, av*·sw* DLL, LICENSE.txt, THIRD-PARTY-NOTICES.txt)
#   CACHE_DIR=…   받은 소스를 두는 곳(기본 ~/.cache/yangpa-ffmpeg). 고정한 sha256이 맞을 때만 씁니다.
#   WORK_DIR=…    빌드 폴더(기본 <출력 폴더>/work). 성공하면 지우고, KEEP_WORK=1이면 남깁니다.
#   JOBS=…        병렬 수(기본 nproc)
#
# 필요한 것(Ubuntu 24.04): sudo apt-get install -y mingw-w64 nasm cmake ninja-build make pkg-config perl curl xz-utils bzip2
# Ubuntu의 mingw-w64 11 헤더에는 gfxcapture가 쓰는 Windows.Graphics.Capture 인터롭 헤더(IGraphicsCaptureItemInterop,
# IDirect3D11CaptureFramePoolStatics2)가 없습니다. 그래서 mingw-w64 14(헤더·CRT·winpthreads)를 빌드 폴더 안에 세우고,
# Ubuntu의 GCC 13(posix 스레드)을 GCC_EXEC_PREFIX로 그쪽을 보게 돌려 씁니다. 시스템 파일은 건드리지 않습니다.

set -euo pipefail

die() { echo "오류: $*" >&2; exit 1; }
log() { printf '\n==> %s\n' "$*"; }

[[ $# -eq 1 ]] || die "사용법: $0 <출력 폴더>"
mkdir -p "$1"
OUT_DIR=$(cd "$1" && pwd)
CACHE_DIR=${CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/yangpa-ffmpeg}
WORK_DIR=${WORK_DIR:-$OUT_DIR/work}
JOBS=${JOBS:-$(nproc)}
HOST=x86_64-w64-mingw32
mkdir -p "$CACHE_DIR" "$WORK_DIR"
CACHE_DIR=$(cd "$CACHE_DIR" && pwd)
WORK_DIR=$(cd "$WORK_DIR" && pwd)

# ---------------------------------------------------------------------------------------------------------------------
# 고정한 소스. 이름|주소|sha256
# FFmpeg·libaom·mingw-w64는 배포자 서명(.asc)과 맞춰 본 tar입니다. SVT-AV1은 GitLab이 태그에서 묶어 주는 tar라
# 바이트가 바뀌면 sha256이 어긋납니다. 그때는 태그(v4.2.0 = 9292ec8e32bce26f781f277ec8739b53426c4300)와 내용을 비교한 뒤 바꿉니다.
# nv-codec-headers는 FFmpeg 9.0이 요구하는 가장 낮은 판(12.1.14.0)이라 드라이버 531.61부터 NVENC가 됩니다.
FFMPEG_VER=9.0.2
AOM_VER=3.15.2
SVT_VER=4.2.0
NVH_VER=12.1.14.0
MINGW_VER=14.0.0
SOURCES=(
  "ffmpeg-$FFMPEG_VER.tar.xz|https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VER.tar.xz|8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e"
  "libaom-$AOM_VER.tar.gz|https://storage.googleapis.com/aom-releases/libaom-$AOM_VER.tar.gz|67bb54b245f33ed98600e08269e6139986e48114e14042474ddd8885801dfddc"
  "SVT-AV1-v$SVT_VER.tar.gz|https://gitlab.com/AOMediaCodec/SVT-AV1/-/archive/v$SVT_VER/SVT-AV1-v$SVT_VER.tar.gz|c7b13c4a84bd3751aa35fcc72be13e6875467e7c2216879251a486e5b1e4e740"
  "nv-codec-headers-$NVH_VER.tar.gz|https://github.com/FFmpeg/nv-codec-headers/releases/download/n$NVH_VER/nv-codec-headers-$NVH_VER.tar.gz|62b30ab37e4e9be0d0c5b37b8fee4b094e38e570984d56e1135a6b6c2c164c9f"
  "mingw-w64-v$MINGW_VER.tar.bz2|https://downloads.sourceforge.net/project/mingw-w64/mingw-w64/mingw-w64-release/mingw-w64-v$MINGW_VER.tar.bz2|6eaf921d9eb987d3820b364ea9775bc19b965ec81490b6fdd716526c28e1995c"
)

# FFmpeg 설정. 경로를 넣지 않아(헤더·라이브러리 위치는 pkg-config가 줌) DLL에 남는 설정 문자열이 빌드 위치와 상관없습니다.
# --disable-autodetect로 몰래 붙는 것(zlib·schannel·mediafoundation 등)을 막고, 쓰는 것만 다시 켭니다.
FF_CONFIGURE=(
  --enable-cross-compile --target-os=mingw32 --arch=x86_64 --cross-prefix=$HOST-
  --pkg-config=pkg-config --pkg-config-flags=--static
  --extra-ldflags='-static-libgcc -static-libstdc++ -Wl,--no-insert-timestamp'
  --enable-shared --disable-static
  --disable-autodetect --disable-everything
  --disable-debug --disable-doc --disable-ffplay --disable-ffprobe
  --disable-network --disable-swresample
  --enable-w32threads --enable-d3d11va --enable-ffnvcodec --enable-nvenc
  --enable-libaom --enable-libsvtav1
  --enable-encoder=libaom_av1,libsvtav1,av1_nvenc,h264_nvenc,hevc_nvenc
  --enable-decoder=wrapped_avframe
  --enable-filter=gfxcapture,fps,hwdownload,format,scale,color
  --enable-indev=lavfi
  --enable-muxer=flv,null
  --enable-protocol=pipe
)
# configure는 의존성이 빠지면 말없이 끄므로, 앱이 쓰는 것은 켜졌는지 따로 확인합니다.
REQUIRED=(
  CONFIG_GFXCAPTURE_FILTER CONFIG_FPS_FILTER CONFIG_HWDOWNLOAD_FILTER CONFIG_FORMAT_FILTER CONFIG_SCALE_FILTER
  CONFIG_COLOR_FILTER CONFIG_LAVFI_INDEV CONFIG_WRAPPED_AVFRAME_DECODER CONFIG_FLV_MUXER CONFIG_NULL_MUXER
  CONFIG_PIPE_PROTOCOL CONFIG_LIBAOM_AV1_ENCODER CONFIG_LIBSVTAV1_ENCODER CONFIG_AV1_NVENC_ENCODER
  CONFIG_H264_NVENC_ENCODER CONFIG_HEVC_NVENC_ENCODER CONFIG_D3D11VA
)
# 정적으로 들어간 외부 코드 중 고지에 적은 것. 빌드에 다른 third_party가 끼면 멈춰서 고지를 먼저 고치게 합니다.
AOM_THIRD_PARTY=(fastfeat vector x86inc)
SVT_THIRD_PARTY=(fastfeat safestringlib)
# DLL·exe가 불러도 되는 Windows 시스템 DLL(소문자). 그 밖의 DLL(libstdc++-6.dll 같은 런타임)이 보이면 실패로 봅니다.
SYSTEM_DLLS=(kernel32.dll user32.dll advapi32.dll shell32.dll ole32.dll bcrypt.dll psapi.dll msvcrt.dll)

# ---------------------------------------------------------------------------------------------------------------------
need() { command -v "$1" >/dev/null || die "$1 이(가) 없습니다(맨 위 주석의 apt-get 줄 참고)"; }
for t in $HOST-gcc-posix $HOST-g++-posix $HOST-cpp-posix $HOST-ar $HOST-ranlib $HOST-nm $HOST-strip $HOST-windres \
  $HOST-dlltool $HOST-objdump nasm cmake ninja make pkg-config perl curl sha256sum tar xz bzip2 strings; do
  need "$t"
done
$HOST-gcc-posix -v 2>&1 | grep -q '^Thread model: posix' || die "$HOST-gcc-posix가 posix 스레드 모델이 아닙니다"

SRC=$WORK_DIR/src
BUILD=$WORK_DIR/build
TC=$WORK_DIR/toolchain     # 돌려 쓰는 GCC와 새 mingw-w64
SYSROOT=$TC/$HOST
DEPS=$WORK_DIR/deps        # libaom·SVT-AV1·nv-codec-headers 설치 위치
STAMPS=$WORK_DIR/stamps    # 실패 뒤 다시 돌릴 때 끝난 단계를 건너뜀
mkdir -p "$SRC" "$BUILD" "$DEPS" "$STAMPS"
done_stage() { [[ -f $STAMPS/$1 ]]; }
mark() { touch "$STAMPS/$1"; }

# 빌드 결과가 날짜·경로에 덜 매이게 합니다(SVT-AV1의 __DATE__ 등).
export SOURCE_DATE_EPOCH=1767225600 TZ=UTC LC_ALL=C
unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS PKG_CONFIG_PATH PKG_CONFIG_SYSROOT_DIR

# ---------------------------------------------------------------------------------------------------------------------
log "소스 받기·확인 ($CACHE_DIR)"
for s in "${SOURCES[@]}"; do
  IFS='|' read -r name url sha <<<"$s"
  f=$CACHE_DIR/$name
  if [[ ! -f $f ]]; then
    curl -fL --retry 3 --proto '=https' -o "$f.part" "$url"
    # 받은 것은 확인을 거친 뒤에만 캐시 이름으로 둡니다.
    echo "$sha  $f.part" | sha256sum -c --quiet - || { rm -f "$f.part"; die "$name: 받은 파일의 sha256이 고정값과 다릅니다"; }
    mv "$f.part" "$f"
  fi
  echo "$sha  $f" | sha256sum -c --quiet - || die "$name: 캐시 파일의 sha256이 고정값과 다릅니다($f를 지우고 다시 돌리세요)"
  echo "  $name ok"
done

unpack() { # <캐시 파일> <폴더 이름>
  rm -rf "${SRC:?}/$2"
  mkdir -p "$SRC/$2"
  tar -xf "$CACHE_DIR/$1" -C "$SRC/$2" --strip-components=1 --no-same-owner
}

# 정적 라이브러리(CRT·winpthreads·libaom·SVT-AV1) 플래그. assert의 __FILE__ 같은 소스 경로가 빌드 위치에 매이지 않게
# 빌드 폴더를 '.'로 바꿉니다.
DEP_CFLAGS="-O2 -pipe -ffile-prefix-map=$WORK_DIR=."

# ---------------------------------------------------------------------------------------------------------------------
GCC_LIBDIR=$(dirname "$($HOST-gcc-posix -print-libgcc-file-name)")
GCC_VER=${GCC_LIBDIR##*/}
if ! done_stage toolchain; then
  log "툴체인: Ubuntu GCC $GCC_VER + mingw-w64 $MINGW_VER"
  rm -rf "$TC"
  mkdir -p "$TC/bin" "$TC/lib/gcc/$HOST/$GCC_VER" "$SYSROOT"
  # GCC는 GCC_EXEC_PREFIX/<대상>/<판>/../../../../<대상>/{include,lib}에서 헤더·CRT를 찾으므로, 자기 파일을 링크로 모은
  # 폴더를 그 자리에 두면 시스템의 mingw-w64 11 대신 SYSROOT를 봅니다. 공유 런타임(*.dll, *.dll.a, libgcc_s.a)은
  # 빼서 libgcc·libstdc++가 늘 정적으로 들어가게 합니다(빠지면 링크가 실패하지, 몰래 DLL을 부르지 않음).
  for f in "$GCC_LIBDIR"/*; do
    case ${f##*/} in
      *.dll | *.dll.a | libgcc_s.a) ;;
      *) ln -s "$f" "$TC/lib/gcc/$HOST/$GCC_VER/" ;;
    esac
  done
  for t in gcc g++ cpp; do
    printf '#!/usr/bin/env bash\nGCC_EXEC_PREFIX=%q exec %q "$@"\n' "$TC/lib/gcc/" "$(command -v $HOST-$t-posix)" >"$TC/bin/$HOST-$t"
    chmod +x "$TC/bin/$HOST-$t"
  done
  ln -s "$HOST-g++" "$TC/bin/$HOST-c++"
  # windres는 같은 폴더의 <대상>-gcc로 전처리하므로 함께 둡니다.
  for t in ar as ld nm objcopy objdump ranlib strip windres dlltool readelf; do
    ln -s "$(command -v $HOST-$t)" "$TC/bin/$HOST-$t"
  done
  export PATH="$TC/bin:$PATH"

  unpack "mingw-w64-v$MINGW_VER.tar.bz2" mingw-w64
  # CRT는 Ubuntu의 libstdc++·libgcc가 빌드된 것과 같은 msvcrt로 맞춥니다(ucrt로 바꾸면 그 둘과 섞여 깨짐).
  mkdir -p "$BUILD/mingw-headers" "$BUILD/mingw-crt" "$BUILD/winpthreads"
  (cd "$BUILD/mingw-headers" &&
    "$SRC/mingw-w64/mingw-w64-headers/configure" --host=$HOST --prefix="$SYSROOT" \
      --with-default-msvcrt=msvcrt --with-default-win32-winnt=0xa00 >configure.log &&
    make install >make.log)
  (cd "$BUILD/mingw-crt" &&
    CFLAGS=$DEP_CFLAGS "$SRC/mingw-w64/mingw-w64-crt/configure" --host=$HOST --prefix="$SYSROOT" \
      --with-default-msvcrt=msvcrt --disable-lib32 --enable-lib64 >configure.log &&
    make -j"$JOBS" >make.log 2>&1 && make install >install.log 2>&1) || die "mingw-w64 CRT 빌드 실패($BUILD/mingw-crt/make.log)"
  # winpthreads는 정적으로만: libstdc++(posix 스레드)와 libaom이 쓰고, DLL 안에 들어갑니다.
  (cd "$BUILD/winpthreads" &&
    CFLAGS=$DEP_CFLAGS "$SRC/mingw-w64/mingw-w64-libraries/winpthreads/configure" --host=$HOST --prefix="$SYSROOT" \
      --disable-shared --enable-static >configure.log &&
    make -j"$JOBS" >make.log 2>&1 && make install >install.log 2>&1) || die "winpthreads 빌드 실패($BUILD/winpthreads/make.log)"

  # gfxcapture가 쓰는 WinRT 헤더가 실제로 보이는지 먼저 봅니다(configure는 못 찾으면 말없이 필터를 끔). configure와 같이 C로.
  printf '%s\n' '#include <windows.h>' '#include <windows.graphics.capture.h>' '#include <windows.graphics.capture.interop.h>' \
    'IGraphicsCaptureItemInterop *a; __x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSession3 *b;' \
    '__x_ABI_CWindows_CGraphics_CCapture_CIDirect3D11CaptureFramePoolStatics2 *c;' >"$BUILD/wgc-test.c"
  "$TC/bin/$HOST-gcc" -D_WIN32_WINNT=0x0A00 -DWINDOWS_FOUNDATION_UNIVERSALAPICONTRACT_VERSION=0x130000 -DCOBJMACROS \
    -c "$BUILD/wgc-test.c" -o "$BUILD/wgc-test.o" || die "새 mingw-w64 헤더에 gfxcapture가 쓰는 WinRT 인터페이스가 없습니다"
  mark toolchain
fi
export PATH="$TC/bin:$PATH"
export PKG_CONFIG_LIBDIR=$DEPS/lib/pkgconfig

CROSS_CMAKE=$WORK_DIR/cross.cmake
cat >"$CROSS_CMAKE" <<EOF
set(CMAKE_SYSTEM_NAME Windows)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
set(CMAKE_C_COMPILER $TC/bin/$HOST-gcc)
set(CMAKE_CXX_COMPILER $TC/bin/$HOST-g++)
set(CMAKE_RC_COMPILER $TC/bin/$HOST-windres)
set(CMAKE_AR $TC/bin/$HOST-ar CACHE FILEPATH "")
set(CMAKE_RANLIB $TC/bin/$HOST-ranlib CACHE FILEPATH "")
set(CMAKE_ASM_NASM_COMPILER $(command -v nasm))
set(CMAKE_FIND_ROOT_PATH $SYSROOT $DEPS)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
set(CMAKE_EXE_LINKER_FLAGS_INIT "-static-libgcc -static-libstdc++")
set(CMAKE_SHARED_LINKER_FLAGS_INIT "-static-libgcc -static-libstdc++")
EOF
# 빌드에 들어간 third_party 폴더가 고지 목록과 같은지 봅니다(ninja 파일의 컴파일 대상 기준).
check_third_party() { # <build.ninja> <이름> <고지에 적은 폴더...>
  local ninja=$1 what=$2
  shift 2
  local d
  for d in $(grep -o '^build [^:]*third_party/[A-Za-z0-9_]*/' "$ninja" | sed 's#.*third_party/##; s#/$##' | sort -u); do
    [[ " $* " == *" $d "* ]] || die "$what에 고지에 없는 third_party/$d가 들어갑니다(THIRD-PARTY-NOTICES를 먼저 고치세요)"
  done
}

# ---------------------------------------------------------------------------------------------------------------------
if ! done_stage nvcodec; then
  log "nv-codec-headers $NVH_VER"
  unpack "nv-codec-headers-$NVH_VER.tar.gz" nv-codec-headers
  make -C "$SRC/nv-codec-headers" PREFIX="$DEPS" install >/dev/null
  mark nvcodec
fi

if ! done_stage aom; then
  log "libaom $AOM_VER (정적, 인코더만)"
  unpack "libaom-$AOM_VER.tar.gz" aom
  rm -rf "$BUILD/aom"
  # 실시간 전용(CONFIG_REALTIME_ONLY)은 끄지 않습니다: 기본 usage(good)로 여는 확인 인코딩이 실패하게 됨.
  CFLAGS=$DEP_CFLAGS CXXFLAGS=$DEP_CFLAGS cmake -S "$SRC/aom" -B "$BUILD/aom" -G Ninja \
    -DCMAKE_TOOLCHAIN_FILE="$CROSS_CMAKE" -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$DEPS" \
    -DBUILD_SHARED_LIBS=OFF -DAOM_TARGET_CPU=x86_64 -DENABLE_NASM=ON \
    -DENABLE_DOCS=OFF -DENABLE_EXAMPLES=OFF -DENABLE_TESTS=OFF -DENABLE_TESTDATA=OFF -DENABLE_TOOLS=OFF \
    -DCONFIG_AV1_DECODER=0 -DCONFIG_LIBYUV=0 -DCONFIG_WEBM_IO=0 >"$BUILD/aom.configure.log"
  check_third_party "$BUILD/aom/build.ninja" libaom "${AOM_THIRD_PARTY[@]}"
  cmake --build "$BUILD/aom" -j "$JOBS" >"$BUILD/aom.build.log" || die "libaom 빌드 실패($BUILD/aom.build.log)"
  cmake --install "$BUILD/aom" >/dev/null
  mark aom
fi

if ! done_stage svt; then
  log "SVT-AV1 $SVT_VER (정적)"
  unpack "SVT-AV1-v$SVT_VER.tar.gz" svt
  rm -rf "$BUILD/svt"
  CFLAGS=$DEP_CFLAGS CXXFLAGS=$DEP_CFLAGS cmake -S "$SRC/svt" -B "$BUILD/svt" -G Ninja \
    -DCMAKE_TOOLCHAIN_FILE="$CROSS_CMAKE" -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$DEPS" \
    -DBUILD_SHARED_LIBS=OFF -DBUILD_APPS=OFF -DBUILD_TESTING=OFF -DSVT_AV1_LTO=OFF -DENABLE_AVX512=ON \
    -DREPRODUCIBLE_BUILDS=ON >"$BUILD/svt.configure.log"
  check_third_party "$BUILD/svt/build.ninja" SVT-AV1 "${SVT_THIRD_PARTY[@]}"
  cmake --build "$BUILD/svt" -j "$JOBS" >"$BUILD/svt.build.log" || die "SVT-AV1 빌드 실패($BUILD/svt.build.log)"
  cmake --install "$BUILD/svt" >/dev/null
  mark svt
fi

# ---------------------------------------------------------------------------------------------------------------------
FF_DEST=$WORK_DIR/ffmpeg-install
if ! done_stage ffmpeg; then
  log "FFmpeg $FFMPEG_VER"
  unpack "ffmpeg-$FFMPEG_VER.tar.xz" ffmpeg
  # 소스 폴더 안에서 빌드해 __FILE__ 같은 경로가 상대 경로로 남게 합니다.
  (cd "$SRC/ffmpeg" && ./configure "${FF_CONFIGURE[@]}" >"$BUILD/ffmpeg.configure.log" 2>&1) ||
    die "FFmpeg configure 실패($SRC/ffmpeg/ffbuild/config.log)"
  missing=()
  for c in "${REQUIRED[@]}"; do
    grep -qh "^#define $c 1\$" "$SRC/ffmpeg/config.h" "$SRC/ffmpeg/config_components.h" || missing+=("$c")
  done
  ((${#missing[@]} == 0)) || die "configure가 꺼 버린 구성 요소: ${missing[*]} ($SRC/ffmpeg/ffbuild/config.log)"
  make -C "$SRC/ffmpeg" -j"$JOBS" >"$BUILD/ffmpeg.build.log" 2>&1 || die "FFmpeg 빌드 실패($BUILD/ffmpeg.build.log)"
  rm -rf "$FF_DEST"
  make -C "$SRC/ffmpeg" install DESTDIR="$FF_DEST" >/dev/null
  mark ffmpeg
fi

# ---------------------------------------------------------------------------------------------------------------------
log "출력 폴더 만들기"
FINAL=$OUT_DIR/ffmpeg
rm -rf "$FINAL"
mkdir -p "$FINAL"
cp "$FF_DEST"/usr/local/bin/ffmpeg.exe "$FF_DEST"/usr/local/bin/*.dll "$FINAL/"
$HOST-strip --strip-all "$FINAL"/*.exe "$FINAL"/*.dll

# pipefail에서 grep -m/-q로 일찍 끊으면 strings가 SIGPIPE로 실패한 것처럼 보여, 끝까지 읽고 거릅니다.
CONFIG_LINE=$(strings -a "$FINAL"/avutil-*.dll | grep -e '--enable-cross-compile' | sort -u || true)
[[ -n $CONFIG_LINE && $CONFIG_LINE != *$'\n'* ]] || die "avutil에서 설정 문자열을 하나로 찾지 못했습니다"
# Ubuntu의 mingw GCC는 판을 '13-posix'처럼만 알려 주므로 패키지 판(13.2.0-…)에서 읽습니다.
GCC_PKG=$(dpkg-query -W -f '${Version}' gcc-mingw-w64-x86-64-posix 2>/dev/null || true)
GCC_FULL=${GCC_PKG%%-*}
GCC_FULL=${GCC_FULL:-$($HOST-gcc-posix -dumpfullversion)}
sha_of() { sha256sum "$CACHE_DIR/$1" | cut -d' ' -f1; }

{
  cat <<EOF
FFmpeg $FFMPEG_VER — GNU Lesser General Public License 2.1 이상(LGPL-2.1-or-later)

이 폴더의 ffmpeg.exe와 av*·sw* DLL은 FFmpeg $FFMPEG_VER 소스를 고치지 않고 아래 설정으로 빌드한 것입니다.
GPL·nonfree 부품은 들어 있지 않습니다. 함께 정적으로 들어간 라이브러리의 고지는 THIRD-PARTY-NOTICES.txt에 있습니다.

소스: https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VER.tar.xz (sha256 $(sha_of "ffmpeg-$FFMPEG_VER.tar.xz"))
빌드 방법: 양파 메신저 저장소의 scripts/build-ffmpeg.sh
설정: $CONFIG_LINE

=====================================================================================================================
EOF
  cat "$SRC/ffmpeg/COPYING.LGPLv2.1"
} >"$FINAL/LICENSE.txt"

section() { # <제목> <라이선스> <소스>
  printf '\n%s\n%s\n라이선스: %s\n소스: %s\n%s\n' \
    '=====================================================================================================================' \
    "$1" "$2" "$3" '---------------------------------------------------------------------------------------------------------------------'
}
file_block() { # <표시 이름> <파일>
  printf '\n--- %s ---\n' "$1"
  cat "$2"
}
comment_block() { # <표시 이름> <C 파일>: 맨 앞 /* … */ 주석(저작권 고지)만
  printf '\n--- %s 머리말 ---\n' "$1"
  sed -n '1,/\*\//p' "$2"
}
{
  cat <<EOF
ffmpeg 폴더의 exe·DLL에 정적으로 들어간 외부 코드의 라이선스 고지입니다. FFmpeg 자체의 라이선스는 LICENSE.txt(LGPL 2.1 이상)입니다.
빌드: scripts/build-ffmpeg.sh (Ubuntu $GCC_FULL mingw-w64 교차 컴파일러 + mingw-w64 $MINGW_VER, msvcrt)
EOF
  section "libaom $AOM_VER (AV1 인코더)" "BSD-2-Clause, Alliance for Open Media Patent License 1.0; 포함된 third_party: BSD-3-Clause(fastfeat), MIT(vector), ISC(x86inc)" \
    "https://storage.googleapis.com/aom-releases/libaom-$AOM_VER.tar.gz (sha256 $(sha_of "libaom-$AOM_VER.tar.gz"))"
  file_block LICENSE "$SRC/aom/LICENSE"
  file_block PATENTS "$SRC/aom/PATENTS"
  for d in "${AOM_THIRD_PARTY[@]}"; do file_block "third_party/$d/LICENSE" "$SRC/aom/third_party/$d/LICENSE"; done

  section "SVT-AV1 $SVT_VER (AV1 인코더)" "BSD-3-Clause-Clear, Alliance for Open Media Patent License 1.0; 포함된 third_party: BSD-3-Clause(fastfeat), MIT(safestringlib), libaom 헤더(aom_ports/x86.h, 위 libaom과 같은 BSD-2-Clause·AOM 특허 라이선스)" \
    "https://gitlab.com/AOMediaCodec/SVT-AV1/-/tree/v$SVT_VER (sha256 $(sha_of "SVT-AV1-v$SVT_VER.tar.gz"))"
  file_block LICENSE.md "$SRC/svt/LICENSE.md"
  file_block PATENTS.md "$SRC/svt/PATENTS.md"
  file_block third_party/fastfeat/LICENSE "$SRC/svt/third_party/fastfeat/LICENSE"
  file_block third_party/safestringlib/LICENSE "$SRC/svt/third_party/safestringlib/LICENSE"

  section "nv-codec-headers $NVH_VER (NVENC 헤더, 드라이버는 실행 중에 불러옴)" "MIT" \
    "https://github.com/FFmpeg/nv-codec-headers/tree/n$NVH_VER (sha256 $(sha_of "nv-codec-headers-$NVH_VER.tar.gz"))"
  comment_block include/ffnvcodec/nvEncodeAPI.h "$SRC/nv-codec-headers/include/ffnvcodec/nvEncodeAPI.h"
  comment_block include/ffnvcodec/dynlink_loader.h "$SRC/nv-codec-headers/include/ffnvcodec/dynlink_loader.h"

  section "mingw-w64 $MINGW_VER (CRT 시작 코드·libmingwex, winpthreads)" "MinGW-w64 runtime(ZPL 2.1·BSD·MIT 등 허용 라이선스, 아래 원문), winpthreads: MIT·BSD-3-Clause" \
    "https://sourceforge.net/projects/mingw-w64/files/mingw-w64/mingw-w64-release/mingw-w64-v$MINGW_VER.tar.bz2 (sha256 $(sha_of "mingw-w64-v$MINGW_VER.tar.bz2"))"
  file_block COPYING.MinGW-w64-runtime/COPYING.MinGW-w64-runtime.txt \
    "$SRC/mingw-w64/COPYING.MinGW-w64-runtime/COPYING.MinGW-w64-runtime.txt"
  file_block mingw-w64-libraries/winpthreads/COPYING "$SRC/mingw-w64/mingw-w64-libraries/winpthreads/COPYING"

  section "GCC $GCC_FULL 런타임 라이브러리(libgcc, libgcc_eh, libstdc++; Ubuntu gcc-mingw-w64 ${GCC_PKG:-?})" \
    "GPL-3.0-or-later WITH GCC-exception-3.1(런타임 예외: 이 라이브러리와 묶은 실행 코드는 원하는 조건으로 배포 가능)" \
    "https://gcc.gnu.org/ (Ubuntu 소스 패키지 gcc-mingw-w64)"
  printf '\n--- GCC RUNTIME LIBRARY EXCEPTION ---\n'
  [[ -f /usr/share/doc/gcc-mingw-w64-base/copyright ]] || die "GCC 런타임 예외 원문(/usr/share/doc/gcc-mingw-w64-base/copyright)이 없습니다"
  sed -n '/^GCC RUNTIME LIBRARY EXCEPTION$/,/^requirements of the license of GCC\.$/p' /usr/share/doc/gcc-mingw-w64-base/copyright
} >"$FINAL/THIRD-PARTY-NOTICES.txt"
grep -q 'No Weakening of GCC Copyleft' "$FINAL/THIRD-PARTY-NOTICES.txt" || die "GCC 런타임 예외 원문을 뽑지 못했습니다"

# ---------------------------------------------------------------------------------------------------------------------
log "확인"
# 1) 불러오는 DLL: 우리 DLL과 Windows 시스템 DLL만.
bad=()
for f in "$FINAL"/*.exe "$FINAL"/*.dll; do
  while read -r dll; do
    l=${dll,,}
    [[ -f $FINAL/$dll || " ${SYSTEM_DLLS[*]} " == *" $l "* ]] || bad+=("${f##*/} → $dll")
  done < <($HOST-objdump -p "$f" | sed -n 's/^\s*DLL Name: //p')
done
((${#bad[@]} == 0)) || die "시스템 DLL이 아닌 것을 부릅니다: ${bad[*]}"
# 2) 라이선스: GPL·nonfree·LGPLv3 설정이 아니고, avutil이 LGPL 2.1+라고 답하는지.
for flag in --enable-gpl --enable-nonfree --enable-version3; do
  [[ " $CONFIG_LINE " != *" $flag "* ]] || die "설정에 $flag가 있습니다"
done
lic=$(strings -a "$FINAL"/*.dll | grep -E 'license: ' | sort -u || true)
[[ -n $lic ]] && ! grep -v -x -E 'lib[a-z]+ license: LGPL version 2\.1 or later' <<<"$lic" ||
  die "라이브러리 라이선스 문자열이 모두 LGPL 2.1+는 아닙니다: $lic"
# 3) 예전 번들에 있던 GPL 부품(FFTW·ZVBI)이나 x264·x265 같은 것이 섞이지 않았는지(라이브러리 함수 이름 기준).
hits=$(strings -a "$FINAL"/*.dll "$FINAL"/*.exe | grep -E 'fftw[fl]?_(plan|execute)|vbi_(decoder|raw)|x264_encoder|x265_api|chromaprint_' || true)
[[ -z $hits ]] || die "GPL 라이브러리 흔적이 있습니다: $(head -n5 <<<"$hits")"

log "설정(ffmpeg -buildconf와 같은 문자열)"
sed 's/ --/\n--/g; ' <<<"$CONFIG_LINE" | sed 's/^/  /'
log "파일과 sha256"
(cd "$FINAL" && sha256sum -- *)
echo "합계: $(du -sh "$FINAL" | cut -f1) ($(du -sb "$FINAL" | cut -f1) 바이트)"

if [[ -z ${KEEP_WORK:-} ]]; then
  rm -rf "$WORK_DIR"
fi
log "완료: $FINAL"
