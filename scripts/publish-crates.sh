#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> Verifying workspace packaging for crates.io (dry-run)..."
cargo publish --workspace --dry-run

CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
if [[ -z "${CARGO_REGISTRY_TOKEN:-}" && ! -f "$CARGO_HOME_DIR/credentials.toml" && ! -f "$CARGO_HOME_DIR/credentials" ]]; then
  echo ""
  echo "crates.io credentials not found in $CARGO_HOME_DIR/credentials.toml or CARGO_REGISTRY_TOKEN."
  echo "Run 'cargo login' with an API token from https://crates.io/settings/tokens, then re-run:"
  echo "  ./scripts/publish-crates.sh"
  exit 1
fi

echo "==> Publishing all workspace crates to crates.io in dependency order..."
cargo publish --workspace "$@"
