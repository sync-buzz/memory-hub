#!/bin/sh
# build.sh — build memory-hub release binary from source
#
# Usage:
#   ./build.sh                     # build for host target
#   ./build.sh --target <triple>   # cross-compile
#   ./build.sh --release           # release build (default)
#   ./build.sh --debug             # debug build
#   ./build.sh --help

set -eu

# ── Defaults ──────────────────────────────────────────────────────────────
TARGET=""
PROFILE="release"

# ── Help ──────────────────────────────────────────────────────────────────
usage() {
    cat <<'EOF'
memory-hub builder

Usage:
  ./build.sh [OPTIONS]

Options:
  --target <triple>   Cross-compile for target (e.g. aarch64-apple-darwin)
  --release           Release profile (default)
  --debug             Debug profile
  --help              Show this help message

Output:
  target/<profile>/memory-hub            (host target)
  target/<triple>/<profile>/memory-hub   (cross-compile)
EOF
}

# ── Parse arguments ───────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
    case "$1" in
        --target)
            if [ $# -lt 2 ]; then
                echo "build.sh: --target needs a triple" >&2
                exit 1
            fi
            TARGET="$2"
            shift 2
            ;;
        --release)
            PROFILE="release"
            shift
            ;;
        --debug)
            PROFILE="debug"
            shift
            ;;
        --help)
            usage
            exit 0
            ;;
        *)
            echo "build.sh: unknown option: $1" >&2
            echo "Run with --help for usage." >&2
            exit 1
            ;;
    esac
done

# ── Preconditions ─────────────────────────────────────────────────────────
if ! command -v cargo >/dev/null 2>&1; then
    echo "build.sh: cargo not found in PATH" >&2
    echo "Install Rust: https://rustup.rs" >&2
    exit 1
fi

# Resolve script dir (so it works from any cwd)
script_dir="$(cd "$(dirname "$0")" && pwd)"
cd "$script_dir"

# ── Build ─────────────────────────────────────────────────────────────────
build_args="build"
if [ "$PROFILE" = "release" ]; then
    build_args="$build_args --release"
fi
if [ -n "$TARGET" ]; then
    build_args="$build_args --target $TARGET"
fi

echo "Building memory-hub ($PROFILE)${TARGET:+ for $TARGET}..."
cargo $build_args

# ── Locate binary ─────────────────────────────────────────────────────────
if [ -n "$TARGET" ]; then
    bin_dir="target/$TARGET/$PROFILE"
else
    bin_dir="target/$PROFILE"
fi
binary="$bin_dir/memory-hub"

if [ ! -f "$binary" ]; then
    echo "build.sh: binary not found at $binary" >&2
    exit 1
fi

echo ""
echo "Built: $binary"
"$binary" --version 2>/dev/null || true
echo ""
echo "Install with: ./install.sh"
