#!/usr/bin/env bash
set -euo pipefail

# Get repository root directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_DIR}"

echo "============================================================"
echo " Starting fresh rebuild & install for zsh-flex-history (Rust)"
echo "============================================================"

echo "==> 1. Stopping any running zsh-flex-history daemons..."
# Gracefully terminate any running instances
pkill -TERM -x "zsh-flex-history" 2>/dev/null || true
pkill -TERM -x "zsh_flex_history" 2>/dev/null || true
pkill -TERM -f "zsh-flex-history.*--daemon" 2>/dev/null || true
pkill -TERM -f "zsh_flex_history.*--daemon" 2>/dev/null || true
pkill -TERM -f "python.*zsh_flex_history" 2>/dev/null || true
sleep 0.2

# Force kill any lingering daemons
pkill -9 -x "zsh-flex-history" 2>/dev/null || true
pkill -9 -x "zsh_flex_history" 2>/dev/null || true
pkill -9 -f "zsh-flex-history.*--daemon" 2>/dev/null || true
pkill -9 -f "zsh_flex_history.*--daemon" 2>/dev/null || true
pkill -9 -f "python.*zsh_flex_history" 2>/dev/null || true

# Remove stale sockets from all potential cache/temp directories
rm -f /tmp/zsh-flex-history-*.sock 2>/dev/null || true
if [ -n "${TMPDIR:-}" ]; then
    rm -f "${TMPDIR}"/zsh-flex-history-*.sock "${TMPDIR}"zsh-flex-history-*.sock 2>/dev/null || true
fi
if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    rm -f "${XDG_RUNTIME_DIR}"/zsh-flex-history-*.sock 2>/dev/null || true
fi

for socket_dir in \
    "${XDG_CACHE_HOME:-$HOME/.cache}/zsh-flex-history" \
    "${HOME}/Library/Caches/zsh-flex-history" \
    "${HOME}/Library/Application Support/zsh-flex-history" \
    "${HOME}/.local/state/zsh-flex-history"; do
    if [ -d "${socket_dir}" ]; then
        rm -f "${socket_dir}"/*.sock 2>/dev/null || true
    fi
done
echo "    ✓ Daemons stopped and stale sockets cleaned."

echo "==> 2. Cleaning up legacy Python/uv installations if present..."
if command -v uv >/dev/null 2>&1; then
    uv tool uninstall zsh-flex-history 2>/dev/null || true
fi
if command -v pip >/dev/null 2>&1; then
    pip uninstall -y zsh-flex-history 2>/dev/null || true
fi
echo "    ✓ Legacy wrappers cleaned."

echo "==> 3. Compiling pure Rust release binaries..."
cargo build --release
echo "    ✓ Release binaries compiled in target/release/."

echo "==> 4. Installing pure Rust release binaries..."
cargo install --path . --force

BIN_TARGETS=("zsh-flex-history" "zsh-flex-history-init-zsh" "zsh-flex-history-import")

LOCAL_BIN="${HOME}/.local/bin"
if [ -d "${LOCAL_BIN}" ] || [[ ":${PATH}:" == *":${HOME}/.local/bin:"* ]]; then
    mkdir -p "${LOCAL_BIN}"
    for bin_name in "${BIN_TARGETS[@]}"; do
        cp -f "${REPO_DIR}/target/release/${bin_name}" "${LOCAL_BIN}/${bin_name}"
        chmod +x "${LOCAL_BIN}/${bin_name}"
    done
fi

CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
if [ -d "${CARGO_BIN}" ]; then
    for bin_name in "${BIN_TARGETS[@]}"; do
        cp -f "${REPO_DIR}/target/release/${bin_name}" "${CARGO_BIN}/${bin_name}"
        chmod +x "${CARGO_BIN}/${bin_name}"
    done
fi
echo "    ✓ Installed: ${BIN_TARGETS[*]}"

echo "==> 5. Initializing Zsh shell hook..."
"${REPO_DIR}/target/release/zsh-flex-history-init-zsh" >/dev/null || true
echo "    ✓ Zsh hook initialized at ${XDG_CONFIG_HOME:-$HOME/.config}/zsh-flex-history/hook.zsh"

echo ""
echo "============================================================"
echo " Installation Complete!"
echo " Binaries: zsh-flex-history, zsh-flex-history-init-zsh, zsh-flex-history-import"
echo ""
echo " To start using immediately in your current shell:"
echo "   source \"\${XDG_CONFIG_HOME:-\$HOME/.config}/zsh-flex-history/hook.zsh\""
echo " Or open a new terminal window."
echo "============================================================"
