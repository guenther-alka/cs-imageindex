#!/bin/bash
# ============================================================================
# cs-imageindex_omnios_1a.sh
# Build cs-imageindex on OmniOS / Illumos -- clean start
#
# Modeled on rustfs_omnios_1a.sh's proven pattern (system pkgs -> Rust check
# -> swap check -> fresh clone -> build), but far simpler: cs-imageindex has
# none of RustFS's exotic deps (no pulsar/mimalloc/jemalloc/aws-lc-rs), so no
# source patching is expected. NOT YET VALIDATED on real OmniOS hardware --
# omni58.189 is currently shut down. First real run will likely surface 1-2
# small issues (protoc for tract-onnx's prost/protobuf codegen, and/or
# ureq's TLS backend choice are the two most likely candidates -- flagged
# below) the same way RustFS's script needed ~13 iterations to get clean.
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
# Same requirement RustFS hit for pulsar -- installing proactively rather
# than waiting for the "protoc not found" cargo build error.
pkg install -q ooce/developer/protobuf  2>/dev/null || true

for cmd in gcc git curl protoc; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "ERROR: $cmd not found. Aborting."
        exit 1
    fi
done
echo "  GCC:    $(gcc --version | head -1)"
echo "  protoc: $(protoc --version)"
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
#    ample, but RustFS's experience showed illumos build links can spike --
#    check cheaply and only add swap if genuinely short)
# ---------------------------------------------------------------------------
echo "[3/6] Checking swap..."

SWAP_KB=$(swap -l 2>/dev/null | awk 'NR>1 {sum+=$4} END {print sum+0}')
SWAP_GB=$((SWAP_KB / 1024 / 1024))
echo "  Current swap: ${SWAP_GB}GB"

if [ "$SWAP_GB" -lt 2 ]; then
    echo "  WARNING: less than 2GB swap. If the build OOMs during linking,"
    echo "           add swap the same way rustfs_omnios_1a.sh does"
    echo "           (zfs create -V <n>g rpool/swap_build && swap -a ...)."
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
# 5. Cargo config (illumos target linker -- same shape as RustFS's, minus
#    the tokio_unstable/AWS_LC_SYS bits it needed for its own deps)
# ---------------------------------------------------------------------------
echo "[5/6] Writing .cargo/config.toml..."

mkdir -p "$REPO_DIR/.cargo"
cat > "$REPO_DIR/.cargo/config.toml" << 'CARGOEOF'
[target.x86_64-unknown-illumos]
linker = "gcc"
ar = "ar"
CARGOEOF

echo "  -> .cargo/config.toml written"
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
    echo " Known first-run candidates (per RustFS precedent -- neither"
    echo " confirmed nor ruled out yet, since this hasn't run on real"
    echo " OmniOS hardware):"
    echo "   - prost-build/protoc: if it still can't find protoc despite"
    echo "     step 1, set PROTOC=/opt/ooce/bin/protoc explicitly."
    echo "   - ureq's TLS backend: if it pulls in openssl-sys/native-tls"
    echo "     instead of rustls, that's the aws-lc-rs-style illumos pain"
    echo "     RustFS hit -- pin ureq's rustls feature explicitly in"
    echo "     Cargo.toml instead of the default TLS feature."
    echo "============================================================"
    exit 1
fi

BINARY="$REPO_DIR/target/release/cs-imageindex"

if [ -f "$BINARY" ]; then
    echo ""
    echo "============================================================"
    echo " BUILD SUCCESSFUL  [1a]"
    echo " Started:  $START_TS"
    echo " Finished: $(date '+%Y-%m-%d %H:%M:%S %Z')"
    echo " Binary:   $BINARY"
    echo " Size:     $(ls -lh $BINARY | awk '{print $5}')"
    echo "============================================================"
else
    echo "BUILD FAILED: binary not found."
    exit 1
fi
