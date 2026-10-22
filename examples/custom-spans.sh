#!/usr/bin/env bash
# examples/custom-spans.sh
# Demonstrates emitting custom spans, instants, counters, and tasks into tachograph.
set -euo pipefail

TACHO="${TACHO_BIN:-$(command -v tacho || echo /tmp/tacho-target-ring/debug/tacho)}"

echo "=== tachograph Custom Spans & Metrics Demo ==="
echo "Using binary: ${TACHO}"

# 1. Ensure daemon is accessible
if ! "${TACHO}" status >/dev/null 2>&1; then
    echo "Starting tacho daemon..."
    "${TACHO}" daemon &
    sleep 0.5
fi

TRACK="custom:pipeline"

# 2. Emit root span
echo "Emitting root span 'data-migration' on ${TRACK}..."
"${TACHO}" emit span begin "${TRACK}" "data-migration" --arg version="v2" --arg batch_size="5000"

# 3. Emit nested span
echo "Emitting nested span 'schema-validation'..."
"${TACHO}" emit span begin "${TRACK}" "schema-validation"
sleep 0.1

# 4. Emit instant checkpoint
"${TACHO}" emit instant "${TRACK}" "checkpoint" --arg status="verified" --arg rows="5000"
sleep 0.1

# End nested span
"${TACHO}" emit span end "${TRACK}"

# 5. Emit custom counter metrics
echo "Emitting counter metrics..."
"${TACHO}" emit counter "${TRACK}/records_processed" 5000 --unit "records"
"${TACHO}" emit counter "${TRACK}/elapsed_ms" 240 --unit "ms"

# 6. End root span
"${TACHO}" emit span end "${TRACK}"

echo "Custom spans emitted successfully."
echo "Exporting trace to verify custom spans..."
"${TACHO}" trace -o /tmp/custom-spans.perfetto-trace
echo "Exported to /tmp/custom-spans.perfetto-trace"
