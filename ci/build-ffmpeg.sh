#!/bin/bash
# =============================================================================
# ci/build-ffmpeg.sh -- build a minimal, static, LGPL-only ffmpeg/ffprobe for
# the current platform and copy the binaries into <outdir>.
#
# Mirrors the illumos recipe (illumos/cs-imageindex_omnios_1a.sh): only the
# demuxers/decoders/encoders cs-imageindex needs for video (frame extraction
# via ffmpeg + container probing via ffprobe) -- no GPL components, no
# external codec libraries, no avdevice, no network.
#
# Supported hosts: linux (gcc), darwin (clang), windows (MSYS2/MinGW-w64).
# Build-time deps (installed by the CI workflow, not here):
#   linux   -> gcc make (nasm optional)
#   darwin  -> clang make (nasm optional)
#   windows -> mingw-w64-x86_64-toolchain make (nasm optional, MSYS2 shell)
#
# Usage:  bash ci/build-ffmpeg.sh <outdir>
# Env:    FFMPEG_VERSION (default 7.1.2)
# =============================================================================

set -e
set -o pipefail

OUT="$1"
[ -n "$OUT" ] || { echo "usage: build-ffmpeg.sh <outdir>"; exit 1; }

FFMPEG_VERSION="${FFMPEG_VERSION:-7.1.2}"
FFMPEG_URL="${FFMPEG_URL:-https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "[ffmpeg] building $FFMPEG_VERSION for $(uname -s)/$(uname -m)"

case "$(uname -s)" in
  MINGW*|MSYS*) HOST=windows ;;
  Darwin)       HOST=darwin ;;
  Linux)        HOST=linux ;;
  *) echo "[ffmpeg] unsupported host: $(uname -s)"; exit 1 ;;
esac

mkdir -p "$OUT"

# ---- download + extract pinned source -------------------------------------
cd "$WORK"
curl -sSL "$FFMPEG_URL" -o ffmpeg.tar.xz
tar -xf ffmpeg.tar.xz
cd "ffmpeg-${FFMPEG_VERSION}"

# ---- configure: minimal static LGPL build ----------------------------------
CC_FLAGS=""
case "$HOST" in
  windows) CC_FLAGS="--cc=x86_64-w64-mingw32-gcc" ;;
  darwin)  CC_FLAGS="--cc=clang" ;;
  linux)   CC_FLAGS="--cc=gcc" ;;
esac

./configure \
  --prefix="$WORK/out" \
  $CC_FLAGS \
  --disable-shared --enable-static \
  --disable-programs --enable-ffmpeg --enable-ffprobe \
  --disable-avdevice --disable-network \
  --disable-gpl --disable-nonfree \
  --disable-doc \
  > configure.log 2>&1

# ---- build -----------------------------------------------------------------
NCORES="$(nproc 2>/dev/null || getconf NPROCESSORS_ONLN 2>/dev/null || echo 2)"
make -j"$NCORES" > build.log 2>&1

# ---- install binaries into outdir ------------------------------------------
if [ "$HOST" = windows ]; then
  cp ffmpeg.exe ffprobe.exe "$OUT/"
else
  cp ffmpeg ffprobe "$OUT/"
fi

echo "[ffmpeg] done ($HOST):"
ls -la "$OUT/"
