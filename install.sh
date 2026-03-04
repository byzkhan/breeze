#!/bin/bash
set -e

# Breeze — terminal coding agent
# Install: curl -fsSL https://raw.githubusercontent.com/byzkhan/breeze/main/install.sh | bash

REPO="https://github.com/byzkhan/breeze.git"
INSTALL_DIR="$HOME/.breeze/bin"
BREEZE_DIR="$HOME/.breeze"

# ── Colors ──────────────────────────────────────────────────────

BOLD='\033[1m'
CYAN='\033[36m'
GREEN='\033[32m'
YELLOW='\033[33m'
RED='\033[31m'
RESET='\033[0m'

info()  { echo -e "${CYAN}${BOLD}▸${RESET} $1"; }
ok()    { echo -e "${GREEN}${BOLD}✓${RESET} $1"; }
warn()  { echo -e "${YELLOW}${BOLD}⚠${RESET} $1"; }
fail()  { echo -e "${RED}${BOLD}✗${RESET} $1" >&2; exit 1; }

# ── Platform detection ──────────────────────────────────────────

detect_platform() {
    case "$(uname -s)" in
        Darwin) OS="macos" ;;
        Linux)  OS="linux" ;;
        MINGW*|MSYS*|CYGWIN*) fail "Windows is not supported. Use WSL." ;;
        *) fail "Unsupported OS: $(uname -s)" ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64)   ARCH="x64" ;;
        arm64|aarch64)  ARCH="arm64" ;;
        *) fail "Unsupported architecture: $(uname -m)" ;;
    esac

    info "Detected platform: $OS ($ARCH)"
}

# ── Rust toolchain ──────────────────────────────────────────────

ensure_rust() {
    if command -v cargo >/dev/null 2>&1; then
        ok "Rust toolchain found: $(rustc --version 2>/dev/null || echo 'unknown')"
        return
    fi

    # Check common install location
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
        export PATH="$HOME/.cargo/bin:$PATH"
        ok "Rust toolchain found at ~/.cargo/bin"
        return
    fi

    info "Rust not found. Installing via rustup..."
    if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
        fail "curl or wget required to install Rust"
    fi

    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --quiet
    export PATH="$HOME/.cargo/bin:$PATH"

    if ! command -v cargo >/dev/null 2>&1; then
        fail "Rust installation failed"
    fi
    ok "Rust installed: $(rustc --version)"
}

# ── Build & install ─────────────────────────────────────────────

install_breeze() {
    info "Building breeze from source (this may take a minute)..."

    # Use cargo install --git to build and install directly
    cargo install --git "$REPO" --root "$BREEZE_DIR" --force 2>&1 | while read -r line; do
        # Show progress without flooding
        case "$line" in
            *Compiling*) echo -e "  ${CYAN}$line${RESET}" ;;
            *Downloading*) echo -e "  ${CYAN}$line${RESET}" ;;
            *Installing*) echo -e "  ${GREEN}$line${RESET}" ;;
            *error*|*Error*) echo -e "  ${RED}$line${RESET}" ;;
        esac
    done

    if [ ! -x "$INSTALL_DIR/breeze" ]; then
        fail "Build failed. Check errors above."
    fi
    ok "breeze built and installed to $INSTALL_DIR/breeze"
}

# ── Shell integration ───────────────────────────────────────────

setup_path() {
    local path_line="export PATH=\"$INSTALL_DIR:\$PATH\""

    # Check if already on PATH
    if echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
        ok "PATH already includes $INSTALL_DIR"
        return
    fi

    local shell_name
    shell_name="$(basename "${SHELL:-/bin/bash}")"

    local profile=""
    case "$shell_name" in
        zsh)
            profile="$HOME/.zshrc"
            ;;
        bash)
            if [ -f "$HOME/.bash_profile" ]; then
                profile="$HOME/.bash_profile"
            else
                profile="$HOME/.bashrc"
            fi
            ;;
        fish)
            # fish uses a different syntax
            local fish_config="$HOME/.config/fish/config.fish"
            mkdir -p "$(dirname "$fish_config")"
            if ! grep -q "$INSTALL_DIR" "$fish_config" 2>/dev/null; then
                echo "fish_add_path $INSTALL_DIR" >> "$fish_config"
                ok "Added breeze to PATH in $fish_config"
            fi
            return
            ;;
        *)
            profile="$HOME/.profile"
            ;;
    esac

    if [ -n "$profile" ]; then
        if ! grep -q "$INSTALL_DIR" "$profile" 2>/dev/null; then
            echo "" >> "$profile"
            echo "# Breeze" >> "$profile"
            echo "$path_line" >> "$profile"
            ok "Added breeze to PATH in $profile"
        else
            ok "PATH entry already in $profile"
        fi
    fi
}

# ── API key setup ───────────────────────────────────────────────

setup_api_key() {
    mkdir -p "$BREEZE_DIR"

    if [ -f "$BREEZE_DIR/api_key" ] && [ -s "$BREEZE_DIR/api_key" ]; then
        ok "API key already configured"
        return
    fi

    if [ -n "$ANTHROPIC_API_KEY" ]; then
        echo "$ANTHROPIC_API_KEY" > "$BREEZE_DIR/api_key"
        chmod 600 "$BREEZE_DIR/api_key"
        ok "API key saved from ANTHROPIC_API_KEY env var"
        return
    fi

    echo ""
    warn "No API key found."
    echo -e "  Get one at: ${CYAN}https://console.anthropic.com/settings/keys${RESET}"
    echo ""
    echo -n "  Paste your API key (or press Enter to skip): "
    read -r api_key

    if [ -n "$api_key" ]; then
        echo "$api_key" > "$BREEZE_DIR/api_key"
        chmod 600 "$BREEZE_DIR/api_key"
        ok "API key saved to ~/.breeze/api_key"
    else
        info "Skipped. Set ANTHROPIC_API_KEY or run: echo 'sk-...' > ~/.breeze/api_key"
    fi
}

# ── Main ────────────────────────────────────────────────────────

main() {
    echo ""
    echo -e "${CYAN}${BOLD}  ╔══════════════════════════════════╗${RESET}"
    echo -e "${CYAN}${BOLD}  ║         breeze installer         ║${RESET}"
    echo -e "${CYAN}${BOLD}  ║    terminal coding agent         ║${RESET}"
    echo -e "${CYAN}${BOLD}  ╚══════════════════════════════════╝${RESET}"
    echo ""

    detect_platform
    ensure_rust
    install_breeze
    setup_path
    setup_api_key

    echo ""
    echo -e "${GREEN}${BOLD}  Installation complete!${RESET}"
    echo ""
    echo -e "  Run ${CYAN}breeze${RESET} to start, or ${CYAN}breeze --harness${RESET} for the pipeline mode."
    echo -e "  Restart your shell or run: ${CYAN}export PATH=\"$INSTALL_DIR:\$PATH\"${RESET}"
    echo ""
}

main "$@"
