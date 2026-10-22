#!/usr/bin/env bash
# tools/system-trace.sh - Start unprivileged traced + traced_probes and clone tacho-system session
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK_DIR="${TACHO_VERIFY_DIR:-/tmp/tacho-verify}"
PERFETTO_DIR="${PERFETTO_DIR:-/usr/local/google/home/zezeozue/dev/perfetto}"
TRACED="${PERFETTO_DIR}/out/release/traced"
PROBES="${PERFETTO_DIR}/out/release/traced_probes"
PERFETTO="${PERFETTO_DIR}/out/release/perfetto"

mkdir -p "${WORK_DIR}"
export PERFETTO_PRODUCER_SOCK_NAME="${WORK_DIR}/perfetto-producer"
export PERFETTO_CONSUMER_SOCK_NAME="${WORK_DIR}/perfetto-consumer"
rm -f "${PERFETTO_PRODUCER_SOCK_NAME}" "${PERFETTO_CONSUMER_SOCK_NAME}"

echo "Starting traced on ${PERFETTO_PRODUCER_SOCK_NAME}..."
"${TRACED}" > "${WORK_DIR}/traced.log" 2>&1 &
TRACED_PID=$!
sleep 1

echo "Starting traced_probes..."
"${PROBES}" > "${WORK_DIR}/traced_probes.log" 2>&1 &
PROBES_PID=$!
sleep 1

cleanup() {
    echo "Cleaning up traced and traced_probes..."
    kill -9 "${PROBES_PID}" "${TRACED_PID}" 2>/dev/null || true
    pkill -9 -x traced 2>/dev/null || true
    pkill -9 -x traced_probes 2>/dev/null || true
    pkill -9 -x perfetto 2>/dev/null || true
}
trap cleanup EXIT

echo "Starting background tacho-system tracing session..."
"${PERFETTO}" \
  --background \
  --config "${SCRIPT_DIR}/system-default.cfg" \
  -o "${WORK_DIR}/system_bg.pftrace" \
  --txt > "${WORK_DIR}/perfetto_session.log" 2>&1

sleep 2

OUT_FILE="${1:-${WORK_DIR}/system_clone.pftrace}"
echo "Cloning session by name tacho-system to ${OUT_FILE}..."
"${PERFETTO}" \
  --clone-by-name tacho-system \
  -o "${OUT_FILE}"

echo "Cloned successfully: $(ls -lh "${OUT_FILE}")"
