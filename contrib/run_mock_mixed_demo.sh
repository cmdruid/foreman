#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

export RUN_MOCK_DEMO_MODE="mixed"

exec "$SCRIPT_DIR/run_mock_demo.sh" "$@"
