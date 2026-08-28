#!/bin/bash
# ============================================================================
# cs-imageindex_omnios_1a.sh
# Build cs-imageindex on OmniOS / Illumos -- clean start
#
# Modeled on rustfs_omnios_1a.sh's proven pattern (system pkgs -> Rust check
# -> swap check -> fresh clone -> build), but far simpler: cs-imageindex has
# none of RustFS's exotic deps (no pulsar/mimalloc/jemalloc/aws-lc-rs), so no
# source patching is expected.
#
# v0.3 note: RAW photo support (rawloader/imagepipe) is pure Rust and needs
# nothing extra here. HEIC/HEIF support (the default "heic" cargo feature)
# links the system libheif C library via pkg-config, and video support
# shells out to an external ffmpeg/ffprobe at runtime -- both come from the
# extra.omnios publisher and both install under /opt/ooce, which is NOT on
# illumos's default pkg-config search path or default runtime linker search
# path (/lib:/usr/lib only, per crle). Steps 1, 5 and 6 below account for
# that: PKG_CONFIG_PATH so libheif-sys's build.rs can find libheif.pc at
# build time, and an -R rpath baked in via RUSTFLAGS so the resulting binary
# finds libheif.so.1 at run time without the end user needing to set
# LD_LIBRARY_PATH. Confirmed working on real OmniOS r151058j hardware
# (192.168.2.189) for v0.3.0: build succeeds, `cs-imageindex --version`
# runs standalone, and a synthetic ffmpeg-generated test video indexes
# correctly (duration, creation_time, and ISO-6709 GPS all read correctly).
# HEIC could not be smoke-tested on illumos itself (no heif-enc/
# libheif-examples package available there to synthesize a test file), but
# it shares the identical code path already verified functionally on Linux.
#
# Runtime dependency added by v0.3 (beyond what v0.2 needed): the compiled
# binary now requires ooce/library/libheif to be installed on the target
# machine (it is dynamically linked, not bundled) for HEIC files to decode.
# Video support additionally requires ooce/multimedia/ffmpeg to be installed
# and on PATH (or a bundled ffmpeg/ffprobe next to the binary) -- without
# it, video files are skipped gracefully with a note printed at startup.
#
# Usage:
#   bash ./cs-imageindex_omnios_1a.sh
# ============================================================================

set -e
set -o pipefail

if [ -z "${BASH_VERSION:-}" ]; then
    echo "ERROR: Run with bash, not sh:  bash $0"
    exit 1
fi

REPO_DIR="/root/cs-imageindex"
REPO_URL="https://github.com/guenther-alka/cs-imageindex"
LOGFILE="/tmp/cs-imageindex-build.log"
START_TS="$(date '+%Y-%m-%d %H:%M:%S %Z')"

echo "============================================================"
echo " cs-imageindex Build Script 1a for OmniOS / Illumos"
echo " Started: $START_TS"
echo "============================================================"
echo ""

# ---------------------------------------------------------------------------
# 1. System packages
# ---------------------------------------------------------------------------
echo "[1/6] Installing system packages..."

pkg install -q developer/versioning/git 2>/dev/null || true
pkg install -q developer/rust           2>/dev/null || true
pkg install -q developer/gcc            2>/dev/null || true
# protoc: tract-onnx pulls in prost/prost-build for ONNX protobuf parsing.
pkg install -q ooce/developer/protobuf  2>/dev/null || true
# v0.3: pkg-config (to locate libheif.pc), libheif (HEIC/HEIF decoding,
# dynamically linked at build+run time), and ffmpeg (video frame/metadata
# extraction, invoked as an external process at run time only -- not linked).
pkg install -q developer/pkg-config     2>/dev/null || true
pkg install -q ooce/library/libheif     2>/dev/null || true
pkg install -q ooce/multimedia/ffmpeg   2>/dev/null || true

for cmd in gcc git curl protoc pkg-config; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "ERROR: $cmd not found. Aborting."
        exit 1
    fi
done
echo "  GCC:        $(gcc --version | head -1)"
echo "  protoc:     $(protoc --version)"
echo "  pkg-config: $(pkg-config --version)"
echo ""

# ---------------------------------------------------------------------------
# 2. Rust >= 1.75 (tract-onnx 0.21 needs a reasonably recent toolchain)
# ---------------------------------------------------------------------------
echo "[2/6] Checking Rust version..."

export PATH="/opt/ooce/bin:$HOME/.cargo/bin:$PATH"

RUST_OK=0
if command -v rustc >/dev/null 2>&1; then
    RUSTVER=$(rustc --version | grep -oE '[0-9]+\.[0-9]+' | head -1)
    MAJOR=$(echo "$RUSTVER" | cut -d. -f1)
    MINOR=$(echo "$RUSTVER" | cut -d. -f2)
    if [ "$MAJOR" -gt 1 ] || { [ "$MAJOR" -eq 1 ] && [ "$MINOR" -ge 75 ]; }; then
        RUST_OK=1
    fi
fi

if [ "$RUST_OK" -eq 0 ]; then
    echo "  -> System Rust missing/too old, installing via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
        sh -s -- -y --default-toolchain stable --no-modify-path
    source "$HOME/.cargo/env"
fi

echo "  Rust:  $(rustc --version)"
echo "  Cargo: $(cargo --version)"
echo ""

# ---------------------------------------------------------------------------
# 3. Swap check (tract-onnx's dependency tree is small; 4GB should be
#    ample, but illumos build links can spike -- check cheaply and only
#    add swap if genuinely short)
# ---------------------------------------------------------------------------
echo "[3/6] Checking swap..."

SWAP_KB=$(swap -l 2>/dev/null | awk 'NR>1 {sum+=$4} END {print sum+0}')
SWAP_GB=$((SWAP_KB / 1024 / 1024))
echo "  Current swap: ${SWAP_GB}GB"

if [ "$SWAP_GB" -lt 2 ]; then
    echo "  WARNING: less than 2GB swap. If the build OOMs during linking,"
    echo "           add swap (zfs create -V <n>g rpool/swap_build && swap -a ...)."
fi
echo ""

# ---------------------------------------------------------------------------
# 4. Delete old build dir, clone fresh
# ---------------------------------------------------------------------------
echo "[4/6] Deleting old build directory and cloning fresh..."

cd "$HOME" 2>/dev/null || cd /

if [ -d "$REPO_DIR" ]; then
    echo "  -> Removing $REPO_DIR ..."
    rm -rf "$REPO_DIR"
fi

echo "  -> Cloning $REPO_URL ..."
git clone "$REPO_URL" "$REPO_DIR"
cd "$REPO_DIR"
echo "  -> Commit: $(git log --oneline -1)"
echo ""

# ---------------------------------------------------------------------------
# 5. Cargo config (illumos target linker) + libheif env vars
# ---------------------------------------------------------------------------
echo "[5/6] Writing .cargo/config.toml..."

mkdir -p "$REPO_DIR/.cargo"
cat > "$REPO_DIR/.cargo/config.toml" << 'CARGOEOF'
[target.x86_64-unknown-illumos]
linker = "gcc"
ar = "ar"
CARGOEOF

echo "  -> .cargo/config.toml written"

# v0.3: extra.omnios packages (libheif) install under /opt/ooce, which is
# not on the default pkg-config search path -- point pkg-config at it so
# libheif-sys's build.rs can find libheif.pc. Also bake an -R rpath into
# the binary via RUSTFLAGS so it finds libheif.so.1 at run time without
# requiring LD_LIBRARY_PATH to be set for end users.
export PKG_CONFIG_PATH="/opt/ooce/lib/amd64/pkgconfig:/opt/ooce/lib/pkgconfig"
export RUSTFLAGS="-C link-args=-Wl,-R/opt/ooce/lib/amd64"
echo "  -> PKG_CONFIG_PATH=$PKG_CONFIG_PATH"
echo "  -> RUSTFLAGS=$RUSTFLAGS"
echo ""

# ---------------------------------------------------------------------------
# 6. Build
# ---------------------------------------------------------------------------
echo "[6/6] Building cs-imageindex (release)..."
echo "  -> Log: $LOGFILE"
echo ""
echo "=== Build start: $(date) ===" > "$LOGFILE"

cd "$REPO_DIR"

if ! cargo build --release 2>&1 | tee -a "$LOGFILE"; then
    echo ""
    echo "============================================================"
    echo " BUILD FAILED"
    echo " Log: $LOGFILE"
    echo " Last errors:"
    grep "^error" "$LOGFILE" | tail -20
    echo "============================================================"
    echo ""
    echo " Known candidates:"
    echo "   - prost-build/protoc: if it still can't find protoc despite"
    echo "     step 1, set PROTOC=/opt/ooce/bin/protoc explicitly."
    echo "   - ureq's TLS backend: if it pulls in openssl-sys/native-tls"
    echo "     instead of rustls, pin ureq's rustls feature explicitly in"
    echo "     Cargo.toml instead of the default TLS feature."
    echo "   - libheif-sys / pkg-config: if it still can't find libheif.pc,"
    echo "     run 'find / -name libheif.pc' and adjust PKG_CONFIG_PATH"
    echo "     above to match (paths can shift between OmniOS releases)."
    echo "     As a last resort, build with --no-default-features to skip"
    echo "     HEIC support entirely (HEIC files are then just skipped)."
    echo "============================================================"
    exit 1
fi

BINARY="$REPO_DIR/target/release/cs-imageindex"

if [ -f "$BINARY" ]; then
    echo ""
    echo "============================================================"
    echo " BUILD SUCCESSFUL  [1a]"
    echo " Started: $START_TS"
    echo " Finished: $(date '+%Y-%m-%d %H:%M:%S %Z')"
    echo " Binary:   $BINARY"
    echo " Size:     $(ls -lh $BINARY | awk '{print $5}')"
    echo ""
    echo " Runtime dependencies on this machine (dynamically linked/"
    echo " shelled out to, not bundled): ooce/library/libheif (HEIC),"
    echo " ooce/multimedia/ffmpeg (video, optional -- skipped gracefully"
    echo " if absent)."
    echo "============================================================"
else
    echo "BUILD FAILED: binary not found."
    exit 1
fi
