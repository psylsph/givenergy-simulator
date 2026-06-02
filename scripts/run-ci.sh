#!/bin/bash
# Headless CI regression runner for givenergy-simulator.
# Runs all scenario YAML files and checks assertions pass.
#
# Usage: ./scripts/run-ci.sh
#
# Exit code 0 = all scenarios pass, non-zero = failure.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
SCENARIOS_DIR="$PROJECT_DIR/examples"
OUTPUT_DIR=$(mktemp -d)

echo "=== GivEnergy Plant Simulator CI Regression ==="
echo "Output dir: $OUTPUT_DIR"
echo ""

# Build first
echo "--- Building ---"
cargo build --release --bin sim-api 2>&1
echo ""

PASS=0
FAIL=0
TOTAL=0

for scenario in "$SCENARIOS_DIR"/*.yaml; do
    name=$(basename "$scenario" .yaml)
    TOTAL=$((TOTAL + 1))
    echo "--- Running: $name ---"
    
    if cargo run --release --bin sim-api -- run "$scenario" --output "$OUTPUT_DIR/$name" 2>&1; then
        echo "  ✓ $name PASSED"
        PASS=$((PASS + 1))
    else
        echo "  ✗ $name FAILED"
        FAIL=$((FAIL + 1))
    fi
done

echo ""
echo "=== Results ==="
echo "Total: $TOTAL  Passed: $PASS  Failed: $FAIL"

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "FAILED scenarios:"
    for scenario in "$SCENARIOS_DIR"/*.yaml; do
        name=$(basename "$scenario" .yaml)
        if [ -f "$OUTPUT_DIR/$name/${name}.xml" ]; then
            echo "  - $name (see $OUTPUT_DIR/$name/${name}.xml)"
        fi
    done
fi

# Cleanup
rm -rf "$OUTPUT_DIR"

exit $FAIL
