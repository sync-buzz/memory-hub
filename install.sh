#!/bin/sh
# install.sh — install memory-hub from a local build
#
# Prerequisites:
#   ./build.sh   (or ./build.sh --release)
#
# Usage:
#   ./install.sh                                # install to ~/.local/bin
#   ./install.sh --install-dir /custom/path
#   ./install.sh --model bge-m3
#   ./install.sh --skip-model                   # binary only
#   ./install.sh --uninstall
#   ./install.sh --help

set -eu

# ── Defaults ──────────────────────────────────────────────────────────────
INSTALL_DIR="${MEMORY_HUB_INSTALL_DIR:-$HOME/.local/bin}"
MODEL=""
SKIP_MODEL=false
UNINSTALL=false

# ── Help ──────────────────────────────────────────────────────────────────
usage() {
    cat <<'EOF'
memory-hub installer

Installs a locally built binary. Run ./build.sh first.

Usage:
  ./install.sh [OPTIONS]

Options:
  --install-dir <path>  Directory to install memory-hub binary (default: ~/.local/bin)
  --model <id>          Model to download on first run (default: platform default)
  --skip-model          Skip model download (binary only)
  --uninstall           Remove memory-hub binary (models and data are preserved)
  --help                Show this help message

Environment:
  MEMORY_HUB_INSTALL_DIR  Override install directory (same as --install-dir)

Examples:
  ./install.sh
  ./install.sh --model bge-m3
  ./install.sh --uninstall
EOF
}

# ── Parse arguments ───────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
    case "$1" in
        --install-dir)
            INSTALL_DIR="$2"
            shift 2
            ;;
        --model)
            MODEL="$2"
            shift 2
            ;;
        --skip-model)
            SKIP_MODEL=true
            shift
            ;;
        --uninstall)
            UNINSTALL=true
            shift
            ;;
        --help)
            usage
            exit 0
            ;;
        *)
            echo "install.sh: unknown option: $1" >&2
            echo "Run with --help for usage." >&2
            exit 1
            ;;
    esac
done

# ── Platform detection ────────────────────────────────────────────────────
detect_platform() {
    os_raw="$(uname -s)"
    arch_raw="$(uname -m)"

    case "$os_raw" in
        Darwin) os="apple-darwin" ;;
        Linux)  os="unknown-linux-gnu" ;;
        *)
            echo "install.sh: unsupported OS: $os_raw" >&2
            echo "Supported: Darwin, Linux" >&2
            exit 1
            ;;
    esac

    case "$arch_raw" in
        arm64|aarch64) arch="aarch64" ;;
        x86_64|amd64)  arch="x86_64" ;;
        *)
            echo "install.sh: unsupported architecture: $arch_raw" >&2
            echo "Supported: arm64/aarch64, x86_64/amd64" >&2
            exit 1
            ;;
    esac

    TARGET="${arch}-${os}"
}

# ── Platform default model ────────────────────────────────────────────────
platform_default_model() {
    case "$TARGET" in
        aarch64-apple-darwin) echo "bge-m3" ;;
        *) echo "nomic-embed-text-v1.5" ;;
    esac
}

# ── Uninstall ─────────────────────────────────────────────────────────────
do_uninstall() {
    binary_path="$INSTALL_DIR/memory-hub"
    if [ ! -f "$binary_path" ]; then
        echo "memory-hub is not installed at $binary_path"
        exit 0
    fi

    echo "Removing memory-hub binary from $binary_path"
    rm -f "$binary_path"

    echo ""
    echo "memory-hub binary removed."
    echo "Canonical data (config, models, registry) is preserved."
    echo "To remove everything, run: memory-hub uninstall --purge --yes"
    echo "(before removing the binary, or use the registry file directly)"
}

# ── Main install ──────────────────────────────────────────────────────────
main() {
    detect_platform

    if [ "$UNINSTALL" = true ]; then
        do_uninstall
        exit 0
    fi

    echo "Installing memory-hub for $TARGET..."

    # Resolve script dir (so install works from any cwd)
    script_dir="$(cd "$(dirname "$0")" && pwd)"

    # Locate a built binary. A release archive unpacks the binary next to
    # this script, so that is looked at first: in an archive there is no
    # `target/` and no `build.sh`, and searching only for those made the
    # installer that ships with a release unable to find the binary shipped
    # beside it.
    binary_src=""
    for candidate in \
        "$script_dir/memory-hub" \
        "$script_dir/target/release/memory-hub" \
        "$script_dir/target/debug/memory-hub"; do
        if [ -f "$candidate" ]; then
            binary_src="$candidate"
            break
        fi
    done

    # If not found, try to build.
    if [ -z "$binary_src" ]; then
        echo "No built binary found."
        if [ -f "$script_dir/build.sh" ]; then
            echo "Running ./build.sh ..."
            (cd "$script_dir" && sh ./build.sh) || {
                echo "install.sh: build failed" >&2
                exit 1
            }
            binary_src="$script_dir/target/release/memory-hub"
            [ -f "$binary_src" ] || binary_src="$script_dir/target/debug/memory-hub"
        fi
    fi

    if [ -z "$binary_src" ] || [ ! -f "$binary_src" ]; then
        echo "install.sh: no built binary found and build.sh unavailable" >&2
        echo "Run ./build.sh first, then ./install.sh" >&2
        exit 1
    fi

    echo "Source: $binary_src"
    case "$binary_src" in
        */target/debug/*)
            echo ""
            echo "Warning: installing a debug build. It starts an order of magnitude"
            echo "slower than a release build; run ./build.sh --release for the real thing."
            echo ""
            ;;
    esac

    # Create install directory
    mkdir -p "$INSTALL_DIR"

    # Install binary
    chmod +x "$binary_src"
    cp -f "$binary_src" "$INSTALL_DIR/memory-hub"

    echo "Installed memory-hub to $INSTALL_DIR/memory-hub"

    # Check PATH
    case ":$PATH:" in
        *":$INSTALL_DIR:"*)
            # Already in PATH
            ;;
        *)
            echo ""
            echo "Warning: $INSTALL_DIR is not in your PATH."
            for rcfile in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.profile"; do
                if [ -f "$rcfile" ]; then
                    echo "Add this line to $rcfile:"
                    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
                    break
                fi
            done
            echo ""
            ;;
    esac

    # Verify installation
    if "$INSTALL_DIR/memory-hub" --version >/dev/null 2>&1; then
        version_output=$("$INSTALL_DIR/memory-hub" --version 2>&1 || echo "unknown")
        echo "Verified: $version_output"
    else
        echo "Warning: could not verify installation. The binary may need to be run directly."
    fi

    # Download model
    if [ "$SKIP_MODEL" = false ]; then
        if [ -z "$MODEL" ]; then
            MODEL=$(platform_default_model)
        fi

        echo ""
        echo "Downloading model: $MODEL"
        if "$INSTALL_DIR/memory-hub" model download "$MODEL"; then
            echo "Model downloaded."
            "$INSTALL_DIR/memory-hub" model use "$MODEL" 2>/dev/null || true
        else
            echo "Warning: model download failed. You can download it later with:"
            echo "  memory-hub model download $MODEL"
            echo ""
            echo "MCP will operate in FTS-only (text search) mode without a model."
        fi
    fi

    # Print next steps
    echo ""
    echo "──────────────────────────────────────────────────────"
    echo " memory-hub installed successfully!"
    echo "──────────────────────────────────────────────────────"
    echo ""
    echo "Next steps:"
    echo "  1. Check version:  memory-hub --version"
    echo "  2. List models:    memory-hub model list"
    echo "  3. Run setup:      memory-hub setup"
    echo "  4. Start MCP:      memory-hub mcp"
    echo ""
    echo "MCP config (add to your client):"
    echo '  {'
    echo '    "mcpServers": {'
    echo '      "memory-hub": {'
    echo '        "command": "memory-hub",'
    echo '        "args": ["mcp"]'
    echo '      }'
    echo '    }'
    echo '  }'
    echo ""
}

main "$@"
