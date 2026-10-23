#!/usr/bin/env bash
# Ensures the pinned Perfetto trace_processor_shell (v58.3) is installed in
# ${XDG_CACHE_HOME:-$HOME/.cache}/tacho/trace_processor_shell and prints its path.
set -euo pipefail

PINNED_VERSION="v58.3"
CACHE_ROOT="${XDG_CACHE_HOME:-$HOME/.cache}/tacho"
TARGET_BIN="${CACHE_ROOT}/trace_processor_shell"

if [[ -x "${TARGET_BIN}" ]] && "${TARGET_BIN}" --version 2>&1 | grep -qE "v5[789]\.|v[6-9][0-9]\."; then
    echo "${TARGET_BIN}"
    exit 0
fi

mkdir -p "${CACHE_ROOT}"

# 1. Explicit environment override
for env_var in "${TACHO_TP:-}" "${TACHO_TRACE_PROCESSOR:-}"; do
    if [[ -n "${env_var}" && -x "${env_var}" ]]; then
        cp -f "${env_var}" "${TARGET_BIN}.tmp.$$"
        chmod +x "${TARGET_BIN}.tmp.$$"
        mv -f "${TARGET_BIN}.tmp.$$" "${TARGET_BIN}"
        echo "${TARGET_BIN}"
        exit 0
    fi
done

# 2. Local Perfetto release build if present
if [[ -x "${HOME}/dev/perfetto/out/release/trace_processor_shell" ]]; then
    cp -f "${HOME}/dev/perfetto/out/release/trace_processor_shell" "${TARGET_BIN}.tmp.$$"
    chmod +x "${TARGET_BIN}.tmp.$$"
    mv -f "${TARGET_BIN}.tmp.$$" "${TARGET_BIN}"
    echo "${TARGET_BIN}"
    exit 0
fi

# 3. Download pinned v58.3 from Perfetto official artifacts
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "${OS}-${ARCH}" in
    linux-x86_64|linux-amd64) ARCH_DIR="linux-amd64" ;;
    linux-aarch64|linux-arm64) ARCH_DIR="linux-arm64" ;;
    darwin-arm64|darwin-aarch64) ARCH_DIR="mac-arm64" ;;
    darwin-x86_64|darwin-amd64) ARCH_DIR="mac-amd64" ;;
    *) ARCH_DIR="linux-amd64" ;;
esac

URL="https://commondatastorage.googleapis.com/perfetto-luci-artifacts/${PINNED_VERSION}/${ARCH_DIR}/trace_processor_shell"
TMP_BIN="${TARGET_BIN}.tmp.$$"

if curl -fsSL "${URL}" -o "${TMP_BIN}" 2>/dev/null || curl -fsSL "https://get.perfetto.dev/trace_processor" -o "${TMP_BIN}"; then
    chmod +x "${TMP_BIN}"
    mv -f "${TMP_BIN}" "${TARGET_BIN}"
    echo "${TARGET_BIN}"
    exit 0
fi

rm -f "${TMP_BIN}"
echo "error: failed to fetch trace_processor_shell (${PINNED_VERSION})" >&2
exit 1
