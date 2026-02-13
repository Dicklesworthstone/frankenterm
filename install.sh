#!/usr/bin/env bash
#
# ft (frankenterm) installer - swarm-native terminal platform for AI agent fleets
#
# One-liner install:
#   curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/frankenterm/main/install.sh | bash
#
# Or with specific version:
#   curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/frankenterm/main/install.sh | bash -s -- --version v0.1.0
#
set -euo pipefail

REPO_URL="https://github.com/Dicklesworthstone/frankenterm.git"
BINARY_NAME="ft"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() { echo -e "${BLUE}[INFO]${NC} $*"; }
log_success() { echo -e "${GREEN}[SUCCESS]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*"; }

# Check for required tools
check_requirements() {
    log_info "Checking requirements..."

    if ! command -v cargo &>/dev/null; then
        log_error "Rust/Cargo is required but not installed."
        log_info "Install Rust with: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
        exit 1
    fi

    if ! command -v git &>/dev/null; then
        log_error "Git is required but not installed."
        exit 1
    fi

    log_success "All requirements met"
}

# Install ft
install_ft() {
    log_info "Installing ft (frankenterm)..."

    # Use cargo install from git
    if cargo install --git "$REPO_URL" "$BINARY_NAME" --locked; then
        log_success "ft installed successfully"
    else
        log_warn "Locked install failed, trying without --locked"
        cargo install --git "$REPO_URL" "$BINARY_NAME"
    fi
}

# Verify installation
verify_install() {
    log_info "Verifying installation..."

    if command -v "$BINARY_NAME" &>/dev/null; then
        local version
        version=$("$BINARY_NAME" --version 2>/dev/null || echo "version unknown")
        log_success "ft is installed: $version"

        # Show binary location
        local binary_path
        binary_path=$(command -v "$BINARY_NAME")
        log_info "Binary location: $binary_path"
    else
        log_error "Installation verification failed - ft not found in PATH"
        log_info "Ensure ~/.cargo/bin is in your PATH"
        exit 1
    fi
}

# Show post-install info
show_info() {
    echo ""
    echo "========================================"
    echo "  ft (FrankenTerm) installed!"
    echo "========================================"
    echo ""
    echo "Usage:"
    echo "  ft help          - Show all commands"
    echo "  ft spawn         - Launch agent in terminal"
    echo "  ft list          - List active sessions"
    echo "  ft monitor       - Real-time monitoring"
    echo ""
    echo "Quick start:"
    echo "  ft spawn cc --prompt 'Hello Claude'"
    echo ""
    echo "Documentation:"
    echo "  https://github.com/Dicklesworthstone/frankenterm"
    echo ""
}

main() {
    echo "========================================="
    echo "  ft (FrankenTerm) Installer"
    echo "========================================="
    echo ""

    check_requirements
    install_ft
    verify_install
    show_info
}

main "$@"
