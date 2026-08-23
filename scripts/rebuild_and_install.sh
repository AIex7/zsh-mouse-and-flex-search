#!/usr/bin/env bash
set -euo pipefail

# Get repository root directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_DIR}"

echo "==> 1. Stopping any running zsh-flex-history daemons..."
pkill -f "zsh_flex_history" 2>/dev/null || true
pkill -f "zsh-flex-history" 2>/dev/null || true

# Remove any stale sockets
SOCKET_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/zsh-flex-history"
if [ -d "${SOCKET_DIR}" ]; then
    rm -f "${SOCKET_DIR}"/*.sock 2>/dev/null || true
fi
rm -f /tmp/zsh-flex-history-*.sock 2>/dev/null || true
echo "    ✓ Daemons stopped and stale sockets cleaned."

echo "==> 2. Compiling pure Rust release binaries..."
cargo build --release
echo "    ✓ Release binaries compiled successfully in target/release/."

echo "==> 3. Installing pure Rust binaries..."
cargo install --path . --force
if [ -d "${HOME}/.local/bin" ]; then
    cargo install --path . --force --root "${HOME}/.local"
fi
echo "    ✓ Installation completed successfully (zsh-flex-history, zsh-flex-history-init-zsh, zsh-flex-history-import)."

echo ""
echo "============================================================"
echo " All done! Pure Rust zsh-flex-history is installed."
echo " Open a new terminal tab or run 'source ~/.zshrc' to start."
echo "============================================================"
