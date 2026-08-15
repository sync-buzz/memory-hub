#!/bin/sh
# install.sh — one-liner installation for Git Memory
#
# Usage:
#   curl -fsSL https://git-memory.dev/install.sh | sh
#   curl -fsSL https://git-memory.dev/install.sh | sh -s -- --install-dir /custom/path
#   curl -fsSL https://git-memory.dev/install.sh | sh -s -- --model bge-m3
#   curl -fsSL https://git-memory.dev/install.sh | sh -s -- --skip-model
#   curl -fsSL https://git-memory.dev/install.sh | sh -s -- --uninstall
#   curl -fsSL https://git-memory.dev/install.sh | sh -s -- --help

set -eu

# ── Defaults ──────────────────────────────────────────────────────────────
INSTALL_DIR="${GIT_MEMORY_INSTALL_DIR:-$HOME/.local/bin}"
MODEL=""
SKIP_MODEL=false
UNINSTALL=false
GITHUB_REPO="evseevnn/git-memory"
VERSION="latest"

# ── Help ──────────────────────────────────────────────────────────────────
usage() {
    cat <<'EOF'
git-memory installer

Usage: install.sh [OPTIONS]

Options:
  --install-dir <path>  Directory to install git-memory binary (default: ~/.local/bin)
  --model <id>          Model to download (default: platform default)
  --skip-model          Skip model download (binary only)
  --uninstall           Remove git-memory binary (models and data are preserved)
  --version <ver>       Specific version to install (default: latest)
  --help                Show this help message

Environment:
  GIT_MEMORY_INSTALL_DIR  Override install directory (same as --install-dir)

Examples:
  curl -fsSL https://git-memory.dev/install.sh | sh
  curl -fsSL https://git-memory.dev/install.sh | sh -s -- --model bge-m3
  curl -fsSL https://git-memory.dev/install.sh | sh -s -- --uninstall
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
        --version)
            VERSION="$2"
            shift 2
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

# ── Helper: check if a command exists ─────────────────────────────────────
has_command() {
    command -v "$1" >/dev/null 2>&1
}

# ── Uninstall ─────────────────────────────────────────────────────────────
do_uninstall() {
    binary_path="$INSTALL_DIR/git-memory"
    if [ ! -f "$binary_path" ]; then
        echo "git-memory is not installed at $binary_path"
        exit 0
    fi

    echo "Removing git-memory binary from $binary_path"
    rm -f "$binary_path"

    echo ""
    echo "git-memory binary removed."
    echo "Canonical data (config, models, registry) is preserved."
    echo "To remove everything, run: git memory uninstall --purge --yes"
    echo "(before removing the binary, or use the registry file directly)"
}

# ── Main install ──────────────────────────────────────────────────────────
main() {
    detect_platform

    if [ "$UNINSTALL" = true ]; then
        do_uninstall
        exit 0
    fi

    echo "Installing git-memory for $TARGET..."

    # Resolve version
    if [ "$VERSION" = "latest" ]; then
        if has_command curl; then
            VERSION=$(curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" \
                | grep '"tag_name"' \
                | sed -E 's/.*"v?([^"]+)".*/\1/' \
                || echo "")
        elif has_command wget; then
            VERSION=$(wget -qO- "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" \
                | grep '"tag_name"' \
                | sed -E 's/.*"v?([^"]+)".*/\1/' \
                || echo "")
        else
            echo "install.sh: requires curl or wget" >&2
            exit 1
        fi

        if [ -z "$VERSION" ]; then
            echo "install.sh: could not determine latest version" >&2
            echo "Specify with --version <ver>" >&2
            exit 1
        fi
    fi

    echo "Version: $VERSION"

    # Download URL
    archive_name="git-memory-${TARGET}.tar.gz"
    download_url="https://github.com/${GITHUB_REPO}/releases/download/v${VERSION}/${archive_name}"
    checksum_url="https://github.com/${GITHUB_REPO}/releases/download/v${VERSION}/checksums.txt"

    # Create install directory
    mkdir -p "$INSTALL_DIR"

    # Temporary directory for download
    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    echo "Downloading $archive_name..."

    # Download binary archive
    if has_command curl; then
        curl -fsSL -o "$tmpdir/$archive_name" "$download_url" || {
            echo "install.sh: failed to download binary" >&2
            echo "URL: $download_url" >&2
            exit 1
        }
        # Download checksums
        curl -fsSL -o "$tmpdir/checksums.txt" "$checksum_url" 2>/dev/null || true
    elif has_command wget; then
        wget -q -O "$tmpdir/$archive_name" "$download_url" || {
            echo "install.sh: failed to download binary" >&2
            echo "URL: $download_url" >&2
            exit 1
        }
        wget -q -O "$tmpdir/checksums.txt" "$checksum_url" 2>/dev/null || true
    else
        echo "install.sh: requires curl or wget" >&2
        exit 1
    fi

    # Verify checksum if available
    if [ -f "$tmpdir/checksums.txt" ]; then
        expected_hash=$(grep "$archive_name" "$tmpdir/checksums.txt" | awk '{print $1}' || true)
        if [ -n "$expected_hash" ] && has_command sha256sum; then
            actual_hash=$(sha256sum "$tmpdir/$archive_name" | awk '{print $1}')
            if [ "$actual_hash" != "$expected_hash" ]; then
                echo "install.sh: checksum verification failed!" >&2
                echo "Expected: $expected_hash" >&2
                echo "Actual:   $actual_hash" >&2
                exit 1
            fi
            echo "Checksum verified."
        elif [ -n "$expected_hash" ] && has_command shasum; then
            actual_hash=$(shasum -a 256 "$tmpdir/$archive_name" | awk '{print $1}')
            if [ "$actual_hash" != "$expected_hash" ]; then
                echo "install.sh: checksum verification failed!" >&2
                echo "Expected: $expected_hash" >&2
                echo "Actual:   $actual_hash" >&2
                exit 1
            fi
            echo "Checksum verified."
        else
            echo "Warning: could not verify checksum (sha256sum/shasum not found)."
        fi
    else
        echo "Warning: checksums file not available, skipping verification."
    fi

    # Extract binary
    echo "Extracting..."
    tar -xzf "$tmpdir/$archive_name" -C "$tmpdir"

    # Find the binary in the extracted archive
    binary_src="$tmpdir/git-memory"
    if [ ! -f "$binary_src" ]; then
        # Try common archive layouts
        binary_src=$(find "$tmpdir" -name "git-memory" -type f | head -1 || true)
    fi
    if [ ! -f "$binary_src" ]; then
        echo "install.sh: binary not found in archive" >&2
        exit 1
    fi

    # Install binary
    chmod +x "$binary_src"
    mv "$binary_src" "$INSTALL_DIR/git-memory"

    echo "Installed git-memory to $INSTALL_DIR/git-memory"

    # Check PATH
    case ":$PATH:" in
        *":$INSTALL_DIR:"*)
            # Already in PATH
            ;;
        *)
            echo ""
            echo "Warning: $INSTALL_DIR is not in your PATH."
            # Suggest adding to shell config
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
    if "$INSTALL_DIR/git-memory" --version >/dev/null 2>&1; then
        version_output=$("$INSTALL_DIR/git-memory" --version 2>&1 || echo "unknown")
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
        if "$INSTALL_DIR/git-memory" model download "$MODEL"; then
            echo "Model downloaded."
            # Set as active model
            "$INSTALL_DIR/git-memory" model use "$MODEL" 2>/dev/null || true
        else
            echo "Warning: model download failed. You can download it later with:"
            echo "  git memory model download $MODEL"
            echo ""
            echo "MCP will operate in FTS-only (text search) mode without a model."
        fi
    fi

    # Print next steps
    echo ""
    echo "──────────────────────────────────────────────────────"
    echo " git-memory installed successfully!"
    echo "──────────────────────────────────────────────────────"
    echo ""
    echo "Next steps:"
    echo "  1. Check version:  git memory --version"
    echo "  2. List models:    git memory model list"
    echo "  3. Run setup:      git memory setup"
    echo "  4. Start MCP:      git memory mcp"
    echo ""
    echo "MCP config (add to your client):"
    echo '  {'
    echo '    "mcpServers": {'
    echo '      "git-memory": {'
    echo '        "command": "git-memory",'
    echo '        "args": ["mcp"]'
    echo '      }'
    echo '    }'
    echo '  }'
    echo ""
}

main "$@"
