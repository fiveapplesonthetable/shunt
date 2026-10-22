#!/usr/bin/env bash
# examples/supervise-workers.sh
# Demonstrates supervisor orchestration of multiple worker agents with tachograph.
set -euo pipefail

TACHO="${TACHO_BIN:-$(command -v tacho || echo /tmp/tacho-target-ring/debug/tacho)}"

echo "=== tachograph Supervisor Demo ==="
echo "Using binary: ${TACHO}"

# 1. Ensure tacho daemon is running
if ! "${TACHO}" status >/dev/null 2>&1; then
    echo "Starting tacho daemon..."
    "${TACHO}" daemon &
    sleep 0.5
fi

send_hook() {
    local agent="$1"
    local payload="$2"
    TACHO_AGENT="${agent}" "${TACHO}" hook claude <<< "${payload}"
}

# 2. Start worker-1 session
echo "Starting worker-1 session..."
send_hook "worker-1" '{"session_id":"w1-sess-001","agent":"worker-1","hook_event_name":"SessionStart","cwd":"/workspace","model":"claude-3-5-sonnet"}'
send_hook "worker-1" '{"session_id":"w1-sess-001","agent":"worker-1","hook_event_name":"UserPromptSubmit","prompt":"Inspect repository and run tests"}'

# worker-1 runs View (Read) tool call
send_hook "worker-1" '{"session_id":"w1-sess-001","agent":"worker-1","hook_event_name":"PreToolUse","tool_use_id":"w1-t1","tool_name":"View","tool_input":{"file_path":"src/lib.rs"}}'
send_hook "worker-1" '{"session_id":"w1-sess-001","agent":"worker-1","hook_event_name":"PostToolUse","tool_use_id":"w1-t1","tool_name":"View","tool_input":{"file_path":"src/lib.rs"},"tool_response":{"exit_code":0,"content":"pub fn add(a: i32, b: i32) -> i32 { a + b }"}}'

# worker-1 runs Bash build/test tool call
send_hook "worker-1" '{"session_id":"w1-sess-001","agent":"worker-1","hook_event_name":"PreToolUse","tool_use_id":"w1-t2","tool_name":"Bash","tool_input":{"command":"cargo test -p core"}}'
send_hook "worker-1" '{"session_id":"w1-sess-001","agent":"worker-1","hook_event_name":"PostToolUse","tool_use_id":"w1-t2","tool_name":"Bash","tool_input":{"command":"cargo test -p core"},"tool_response":{"exit_code":0,"stdout":"test result: ok. 14 passed"}}'

# 3. Start worker-2 session and induce LOOP stall
echo "Starting worker-2 session (inducing LOOP stall)..."
send_hook "worker-2" '{"session_id":"w2-sess-001","agent":"worker-2","hook_event_name":"SessionStart","cwd":"/workspace","model":"claude-3-5-sonnet"}'
send_hook "worker-2" '{"session_id":"w2-sess-001","agent":"worker-2","hook_event_name":"UserPromptSubmit","prompt":"Fix failing compilation"}'

# worker-2 repeats identical Bash command 3 times with failures
for i in 1 2 3; do
    send_hook "worker-2" '{"session_id":"w2-sess-001","agent":"worker-2","hook_event_name":"PreToolUse","tool_use_id":"w2-t'"$i"'","tool_name":"Bash","tool_input":{"command":"cargo check --tests"}}'
    send_hook "worker-2" '{"session_id":"w2-sess-001","agent":"worker-2","hook_event_name":"PostToolUse","tool_use_id":"w2-t'"$i"'","tool_name":"Bash","tool_input":{"command":"cargo check --tests"},"tool_response":{"exit_code":1,"stderr":"error[E0425]: cannot find value `foo` in this scope"}}'
done

# 4th identical invocation triggers LOOP stall and remains active in Stalled state
send_hook "worker-2" '{"session_id":"w2-sess-001","agent":"worker-2","hook_event_name":"PreToolUse","tool_use_id":"w2-t4","tool_name":"Bash","tool_input":{"command":"cargo check --tests"}}'

# 4. Supervisor detects stall via watch
echo ""
echo "=== Supervisor Watch (tacho watch --until stalled) ==="
"${TACHO}" watch --until stalled || true

# 5. Inspect fleet status
echo ""
echo "=== Fleet Status (tacho status) ==="
"${TACHO}" status

# 6. Retrieve detailed supervisor report for stalled worker
echo ""
echo "=== Detailed Report for worker-2 (tacho report) ==="
"${TACHO}" report worker-2

# 7. Retrieve detailed supervisor report for worker-1
echo ""
echo "=== Detailed Report for worker-1 (tacho report) ==="
"${TACHO}" report worker-1

# 8. Complete worker-2 tool call
send_hook "worker-2" '{"session_id":"w2-sess-001","agent":"worker-2","hook_event_name":"PostToolUse","tool_use_id":"w2-t4","tool_name":"Bash","tool_input":{"command":"cargo check --tests"},"tool_response":{"exit_code":1}}'

# 9. Export snapshot trace for visual analysis in Perfetto UI
echo ""
echo "=== Exporting Perfetto Trace Snapshot ==="
TRACE_OUT="${TMPDIR:-/tmp}/fleet-demo.perfetto-trace"
"${TACHO}" trace -o "${TRACE_OUT}"
echo "Trace snapshot successfully written to: ${TRACE_OUT}"
echo "Open in https://ui.perfetto.dev to inspect."
