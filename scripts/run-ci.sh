#!/bin/bash
# Headless CI regression runner for givenergy-simulator.
# Runs all scenario YAML files and checks assertions pass.
#
# Usage: ./scripts/run-ci.sh
#
# Environment overrides:
#   CI_OUTPUT_DIR     where scenario output goes
#                     (default: target/ci-scenarios/<timestamp>-<pid>)
#   CI_SCENARIOS_DIR  directory of scenario YAML files (default: examples/)
#
# Output lives on the project disk (not /tmp, which is often a small tmpfs;
# multi-day recordings run to GBs). Each passing scenario's output is
# deleted as soon as it passes, so peak usage is one scenario. Failed
# scenarios' output is KEPT so the JUnit XML referenced in the summary
# exists for inspection.
#
# Exit code 0 = all scenarios pass, non-zero = failure.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
SCENARIOS_DIR="${CI_SCENARIOS_DIR:-$PROJECT_DIR/examples}"
OUTPUT_DIR="${CI_OUTPUT_DIR:-$PROJECT_DIR/target/ci-scenarios/$(date +%Y%m%d-%H%M%S)-$$}"
mkdir -p "$OUTPUT_DIR"

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
FAILED_NAMES=()

for scenario in "$SCENARIOS_DIR"/*.yaml; do
    name=$(basename "$scenario" .yaml)
    TOTAL=$((TOTAL + 1))
    echo "--- Running: $name ---"

    if cargo run --release --bin sim-api -- run "$scenario" --output "$OUTPUT_DIR/$name" 2>&1; then
        echo "  ✓ $name PASSED"
        PASS=$((PASS + 1))
        rm -rf "${OUTPUT_DIR:?}/$name"
    else
        echo "  ✗ $name FAILED"
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$name")
    fi
done

echo ""
echo "=== Results ==="
echo "Total: $TOTAL  Passed: $PASS  Failed: $FAIL"

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "FAILED scenarios:"
    for name in "${FAILED_NAMES[@]}"; do
        echo "  - $name (see $OUTPUT_DIR/$name/${name}.xml)"
    done
fi

# Cleanup: only when everything passed — failed output is kept for triage.
if [ "$FAIL" -eq 0 ]; then
    rm -rf "$OUTPUT_DIR"
else
    echo ""
    echo "Failed scenario output kept in: $OUTPUT_DIR"
fi

exit $FAIL
