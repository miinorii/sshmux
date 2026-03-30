#!/usr/bin/env bash
#
# Run Docker integration tests for sshmux.
#
# Usage:
#   ./tests/run-integration.sh          # build container, run tests, tear down
#   ./tests/run-integration.sh --keep   # leave container running after tests

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DOCKER_DIR="$SCRIPT_DIR/docker"
KEEP=false

if [[ "${1:-}" == "--keep" ]]; then
    KEEP=true
fi

# Generate SSH test key if missing
if [[ ! -f "$DOCKER_DIR/test_key" ]]; then
    echo ">> Generating SSH test key..."
    ssh-keygen -t ed25519 -f "$DOCKER_DIR/test_key" -N "" -q
fi

echo ">> Starting Docker container..."
cd "$DOCKER_DIR"
docker compose up -d --build --wait

# Cleanup function: remove the test SSH config alias and stop container
cleanup() {
    echo ">> Cleaning up SSH config alias..."
    local config="$HOME/.ssh/config"
    if [[ -f "$config" ]]; then
        # Remove the auto-generated test block
        sed -i '/# --- sshmux integration test (auto-generated, safe to delete) ---/,/# --- end sshmux integration test ---/d' "$config" 2>/dev/null || true
    fi
    if [[ "$KEEP" == false ]]; then
        echo ">> Stopping Docker container..."
        cd "$DOCKER_DIR"
        docker compose down
    fi
}
trap cleanup EXIT

echo ">> Running integration tests..."
cd "$SCRIPT_DIR/.."
cargo test --test integration -- --ignored --test-threads=1
