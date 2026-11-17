#!/usr/bin/env bash
#
# Compiles the Rust Worker to Wasm via worker-build.
#
# Cloudflare's Workers Builds image ships Node, Python, PHP, Ruby and Go but no
# Rust toolchain, so the toolchain is installed here on demand. Locally this is
# a no-op after the first run.
#
# Usage: bash scripts/cf-build.sh [--dev]
#
set -euo pipefail

cd "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

# worker-build is pinned to a release from the same era as the `worker` crate in
# Cargo.lock (0.5.0); keep the two in step when bumping either.
WORKER_BUILD_VERSION="${WORKER_BUILD_VERSION:-0.1.3}"

BUILD_PROFILE="--release"
if [ "${1:-}" = "--dev" ]; then
	BUILD_PROFILE="--dev"
fi

if ! command -v cargo >/dev/null 2>&1; then
	echo "==> Installing Rust (rustup, minimal profile)"
	curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
		sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path
fi

# Pick up a pre-existing rustup install without relying on the shell rc files,
# which CI never sources.
if [ -f "$HOME/.cargo/env" ]; then
	# shellcheck source=/dev/null
	. "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"

echo "==> Ensuring wasm32-unknown-unknown target"
rustup target add wasm32-unknown-unknown

if ! command -v worker-build >/dev/null 2>&1; then
	echo "==> Installing worker-build ${WORKER_BUILD_VERSION}"
	cargo install --locked worker-build --version "${WORKER_BUILD_VERSION}"
fi

echo "==> Compiling Worker (worker-build ${BUILD_PROFILE})"
worker-build "${BUILD_PROFILE}"
