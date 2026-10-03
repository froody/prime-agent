#!/usr/bin/env bash
# install-local.sh — Build and install Prime Agent from this local checkout.
#
# Defaults:
#   Payload:   ~/.local/share/prime-agent/
#   Launcher:  ~/.local/bin/prime (and a symlink at ~/.local/bin/prime-agent)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${PRIME_AGENT_PREFIX:-$HOME/.local}"
BIN_NAME="${PRIME_AGENT_BIN_NAME:-prime}"
SKIP_BUILD=0
SKIP_BOOTSTRAP=0

# Colors for terminal output
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  C_RESET="\033[0m"
  C_BOLD="\033[1m"
  C_GREEN="\033[32m"
  C_BLUE="\033[34m"
  C_YELLOW="\033[33m"
  C_RED="\033[31m"
else
  C_RESET=""
  C_BOLD=""
  C_GREEN=""
  C_BLUE=""
  C_YELLOW=""
  C_RED=""
fi

step() {
  printf "${C_BLUE}${C_BOLD}==>${C_RESET} ${C_BOLD}%s${C_RESET}\n" "$*"
}

success() {
  printf "${C_GREEN}${C_BOLD}✓${C_RESET} %s\n" "$*"
}

warn() {
  printf "${C_YELLOW}${C_BOLD}!${C_RESET} %s\n" "$*" >&2
}

error() {
  printf "${C_RED}${C_BOLD}x${C_RESET} %s\n" "$*" >&2
  exit 1
}

usage() {
  cat <<USAGE
Usage: ./install-local.sh [options]

Builds and installs Prime Agent from this local repository.

Options:
  --prefix <dir>     Install prefix (default: ~/.local or \$PRIME_AGENT_PREFIX)
  --name <name>       Launcher command name (default: prime or \$PRIME_AGENT_BIN_NAME)
  --skip-build       Skip 'cargo build' and reuse target/release/prime-agent
  --skip-bootstrap   Skip Python kernel pre-warming (--prime-agent-bootstrap)
  -h, --help         Show this help message
USAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix)
      PREFIX="$2"
      shift 2
      ;;
    --name)
      BIN_NAME="$2"
      shift 2
      ;;
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    --skip-bootstrap)
      SKIP_BOOTSTRAP=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      error "Unknown option: $1 (run with -h for help)"
      ;;
  esac
done

BIN_DIR="${PREFIX}/bin"
SHARE_DIR="${PREFIX}/share/prime-agent"

# 1. Check prerequisites
step "Checking prerequisites"
command -v cargo >/dev/null 2>&1 || error "cargo is required to build prime-agent (see https://rustup.rs)"
command -v python3 >/dev/null 2>&1 || error "python3 is required for release packaging"
if ! command -v uv >/dev/null 2>&1 && [ ! -x "${BIN_DIR}/uv" ] && [ ! -x "$HOME/.cargo/bin/uv" ]; then
  warn "uv was not found on PATH. The Python kernel may need uv installed later (curl -LsSf https://astral.sh/uv/install.sh | sh)"
fi
success "Prerequisites found"

# 2. Build binary
if [ "$SKIP_BUILD" -eq 1 ]; then
  step "Skipping build (--skip-build requested)"
  [ -x "${REPO_ROOT}/target/release/prime-agent" ] || error "target/release/prime-agent does not exist or is not executable"
  success "Reusing existing target/release/prime-agent"
else
  step "Building prime-agent binary (cargo build --release)"
  (cd "$REPO_ROOT" && cargo build --release --locked --bin prime-agent)
  success "Built target/release/prime-agent"
fi

# 3. Generate catalog assets
step "Generating bundled catalog assets"
(cd "$REPO_ROOT" && python3 scripts/release/bundle_catalog.py generate --fixture --out target/catalog-assets)
success "Catalog assets generated at target/catalog-assets"

# 4. Stage packaging layout
step "Assembling release package layout"
(cd "$REPO_ROOT" && python3 scripts/package_release.py --skip-build --catalog-assets target/catalog-assets)

STAGE_DIR="$(find "${REPO_ROOT}/target/release-package" -mindepth 1 -maxdepth 1 -type d -name "prime-agent-*" | head -n 1)"
[ -n "$STAGE_DIR" ] && [ -d "$STAGE_DIR" ] || error "Could not locate staged package directory under target/release-package"
[ -x "${STAGE_DIR}/prime-agent" ] || error "Staged directory missing executable prime-agent"
success "Staged release payload in $(basename "$STAGE_DIR")"

# 5. Install payload to $PREFIX/share/prime-agent
step "Installing payload to ${SHARE_DIR}"
mkdir -p "${PREFIX}/share" "${BIN_DIR}"

# Staged copy to avoid running into partially-copied states
TEMP_SHARE="${SHARE_DIR}.tmp.$$"
rm -rf "$TEMP_SHARE"
cp -R "$STAGE_DIR" "$TEMP_SHARE"

# Write install marker
cat << MARKER > "${TEMP_SHARE}/.prime-agent-install"
install-local.sh
date: $(date -u +%Y-%m-%dT%H:%M:%SZ)
commit: $(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo "unknown")
MARKER

# Atomic replacement
if [ -d "$SHARE_DIR" ]; then
  BACKUP_SHARE="${SHARE_DIR}.old.$$"
  mv "$SHARE_DIR" "$BACKUP_SHARE"
  mv "$TEMP_SHARE" "$SHARE_DIR"
  rm -rf "$BACKUP_SHARE" 2>/dev/null || true
else
  mv "$TEMP_SHARE" "$SHARE_DIR"
fi
success "Payload installed to ${SHARE_DIR}"

# 6. Create launcher in $BIN_DIR/$BIN_NAME
step "Installing launcher to ${BIN_DIR}/${BIN_NAME}"
LAUNCHER_TMP="${BIN_DIR}/.${BIN_NAME}.tmp.$$"
cat << 'EOF' > "$LAUNCHER_TMP"
#!/bin/sh
# prime — launcher for Prime Agent
export PRIME_AGENT_CODING_AGENT_DIR="${PRIME_AGENT_CODING_AGENT_DIR:-$HOME/.prime/agent}"
export PRIME_AGENT_DAEMON_SOCKET="${PRIME_AGENT_DAEMON_SOCKET:-${TMPDIR:-/tmp}/prime-agent-rust-$(id -u)/daemon.sock}"
exec "$(dirname "$0")/../share/prime-agent/prime-agent" "$@"
EOF
chmod 0755 "$LAUNCHER_TMP"
mv -f "$LAUNCHER_TMP" "${BIN_DIR}/${BIN_NAME}"
success "Launcher created at ${BIN_DIR}/${BIN_NAME}"

# Also create/update prime-agent symlink if name is different
if [ "$BIN_NAME" != "prime-agent" ]; then
  ln -sf "${BIN_NAME}" "${BIN_DIR}/prime-agent"
  success "Linked ${BIN_DIR}/prime-agent -> ${BIN_NAME}"
fi

# 7. Pre-warm kernel
if [ "$SKIP_BOOTSTRAP" -eq 0 ]; then
  step "Pre-warming Python kernel venv"
  if "${BIN_DIR}/${BIN_NAME}" --prime-agent-bootstrap >/dev/null 2>&1; then
    success "Python kernel bootstrapped and ready"
  else
    warn "Kernel pre-warming did not finish; the first session will bootstrap on launch"
  fi
fi

# 8. Verify
step "Verifying installation"
INSTALLED_VERSION="$("${BIN_DIR}/${BIN_NAME}" --version 2>/dev/null || echo "unknown")"
success "Prime Agent ${INSTALLED_VERSION} is installed!"

# 9. PATH check and advice
printf "\n"
case ":${PATH}:" in
  *":${BIN_DIR}:"*)
    RESOLVED_BIN="$(command -v "$BIN_NAME" 2>/dev/null || true)"
    if [ "$RESOLVED_BIN" != "${BIN_DIR}/${BIN_NAME}" ]; then
      warn "'${BIN_NAME}' on PATH resolves to: ${RESOLVED_BIN}"
      warn "To use the new installation, ensure ${BIN_DIR} appears before other directories in PATH."
    else
      printf "${C_GREEN}${C_BOLD}Ready to go!${C_RESET} Run: ${C_BOLD}%s${C_RESET}\n" "$BIN_NAME"
    fi
    ;;
  *)
    warn "${BIN_DIR} is not in your PATH."
    printf "Add it to your shell configuration (e.g. ~/.zshrc or ~/.bashrc):\n\n"
    printf "  export PATH=\"%s:\$PATH\"\n\n" "${BIN_DIR}"
    ;;
esac
