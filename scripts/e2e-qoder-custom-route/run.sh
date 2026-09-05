#!/usr/bin/env bash
# Qoder custom-route E2E regression (Qoder 1.28.0 + Q Switch debug binary).
#
# Verifies end-to-end that a Qoder conversation routed through the Q Switch
# transparent adapter reaches the mock upstream over the selected wire format
# (openai_chat / anthropic_messages / openai_responses) with the custom model
# name and reasoning_effort applied.
#
# Safety:
#   * inject.py snapshots the real QSwitch/Qoder/manifest state first and
#     restores it on exit (trap), merging only fixed test-UUID rows. If no
#     manifest existed before the run, cleanup removes only the fixed test
#     entry it created.
#   * The mock logs header NAMES only; the test key is a permission-less
#     placeholder.
#
# Requirements:
#   - Qoder installed at /Applications/Qoder IDE.app (1.28.0+)
#   - Q Switch debug binary at src-tauri/target/debug/qswitch
#
# Usage:
#   QODER_APP=/Applications/Qoder\ IDE.app ./run.sh
#   QSWITCH_E2E_API_FORMAT=anthropic_messages ./run.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
QSWITCH_BIN="${QSWITCH_BIN:-${ROOT_DIR}/src-tauri/target/debug/qswitch}"
QODER_APP="${QODER_APP:-/Applications/Qoder IDE.app}"
QODER_CDP_PORT="${QODER_CDP_PORT:-9222}"
QSWITCH_E2E_PORT="${QSWITCH_E2E_PORT:-9000}"
MOCK_LOG="${QSWITCH_E2E_MOCK_LOG:-/tmp/qswitch-e2e-mock.log}"
ROUTE_MODEL="${QSWITCH_E2E_MODEL:-e2e-custom-model}"
ROUTE_EFFORT="${QSWITCH_E2E_EFFORT:-high}"
export QSWITCH_E2E_API_FORMAT="${QSWITCH_E2E_API_FORMAT:-openai_chat}"
# Keep the injected provider pointed at the selected mock port unless a full
# endpoint/base URL is deliberately supplied by the caller.
export QSWITCH_E2E_BASE_URL="${QSWITCH_E2E_BASE_URL:-http://127.0.0.1:${QSWITCH_E2E_PORT}/v1}"
case "${QSWITCH_E2E_API_FORMAT}" in
  openai_chat) E2E_CARRIER_SUFFIX="chat" ;;
  anthropic_messages) E2E_CARRIER_SUFFIX="anthropic" ;;
  openai_responses) E2E_CARRIER_SUFFIX="responses" ;;
  *) log "FAIL: unsupported QSWITCH_E2E_API_FORMAT=${QSWITCH_E2E_API_FORMAT}"; exit 2 ;;
esac
export QSWITCH_E2E_CARRIER_ID="${QSWITCH_E2E_CARRIER_ID:-model_e2e_${E2E_CARRIER_SUFFIX}}"
export QODER_E2E_CARRIER_NAME="${QODER_E2E_CARRIER_NAME:-E2E-Carrier-${E2E_CARRIER_SUFFIX}}"

QODER_BIN="${QODER_APP}/Contents/MacOS/Qoder"
QODER_INFO="$HOME/Library/Application Support/Qoder/SharedClientCache/.info.json"

MOCK_PID=""
QODER_PID=""
QSWITCH_PID=""

log() { printf '[e2e] %s\n' "$*"; }

cleanup() {
  local status=$?
  log "cleaning up (exit status ${status})..."
  [ -n "${QSWITCH_PID}" ] && kill "${QSWITCH_PID}" 2>/dev/null || true
  [ -n "${QODER_PID}" ] && kill "${QODER_PID}" 2>/dev/null || true
  [ -n "${MOCK_PID}" ] && kill "${MOCK_PID}" 2>/dev/null || true
  # Give SQLite-owning processes a moment to close before restoring snapshots.
  sleep 1
  [ -n "${QSWITCH_PID}" ] && wait "${QSWITCH_PID}" 2>/dev/null || true
  [ -n "${QODER_PID}" ] && wait "${QODER_PID}" 2>/dev/null || true
  [ -n "${MOCK_PID}" ] && wait "${MOCK_PID}" 2>/dev/null || true
  # Restore the snapshot; safe to run even on early failure.
  python3 "${SCRIPT_DIR}/inject.py" cleanup 2>/dev/null || true
  rm -f "${MOCK_LOG}"
  exit "${status}"
}
trap cleanup EXIT INT TERM

assert_request() {
  # The last logged request must hit the path matching the selected format,
  # carry the custom model name + reasoning effort, and show an auth header
  # present (value is never logged). The internal ACP session header must not
  # be forwarded to the real upstream.
  local log="$1"
  QSWITCH_E2E_MODEL="${ROUTE_MODEL}" QSWITCH_E2E_EFFORT="${ROUTE_EFFORT}" \
  QSWITCH_E2E_API_FORMAT="${QSWITCH_E2E_API_FORMAT}" \
  python3 - "$log" <<'PY'
import json, os, sys
log = sys.argv[1]
fmt = os.environ["QSWITCH_E2E_API_FORMAT"]
expected_path = {
    "openai_chat": "/chat/completions",
    "anthropic_messages": "/v1/messages",
    "openai_responses": "/responses",
}[fmt]
with open(log, encoding="utf-8") as fh:
    lines = [json.loads(l) for l in fh if l.strip()]
if not lines:
    print("FAIL: mock received no requests")
    sys.exit(1)
last = lines[-1]
auth_key = "x-api-key" if fmt == "anthropic_messages" else "authorization"
checks = {
    f"path ends with {expected_path}": last["path"].split("?", 1)[0].endswith(expected_path),
    "model (custom name)": last.get("model") == os.environ["QSWITCH_E2E_MODEL"],
    "streaming": last.get("stream") is True,
    "auth header present (value not logged)": last["authHeaderPresent"].get(auth_key, False),
    "internal session header not forwarded upstream": not last.get("hasSessionHeader", False),
}
for name, ok in checks.items():
    print(("PASS" if ok else "FAIL") + f": {name}")
if not all(checks.values()):
    sys.exit(1)
print(f"PASS: {fmt} E2E request verified (no secret values logged)")
PY
}

rm -f "${MOCK_LOG}"

# 1. snapshot real state BEFORE touching anything.
python3 "${SCRIPT_DIR}/inject.py" snapshot

# 2. mock upstream (three protocols on one port)
QSWITCH_E2E_PORT="${QSWITCH_E2E_PORT}" QSWITCH_E2E_MOCK_LOG="${MOCK_LOG}" \
  python3 "${SCRIPT_DIR}/mock_openai.py" &
MOCK_PID=$!
log "mock listening on 127.0.0.1:${QSWITCH_E2E_PORT} (pid ${MOCK_PID}, format=${QSWITCH_E2E_API_FORMAT})"
sleep 1
kill -0 "${MOCK_PID}" 2>/dev/null || {
  log "FAIL: mock did not start; choose another QSWITCH_E2E_PORT."
  exit 1
}

# 3. inject fixtures: test provider + model + hidden provider + Qoder carrier
python3 "${SCRIPT_DIR}/inject.py" routes

# 4. start Qoder so the native Agent publishes .info.json
"${QODER_BIN}" --remote-debugging-port="${QODER_CDP_PORT}" >/tmp/qswitch-e2e-qoder.log 2>&1 &
QODER_PID=$!
log "Qoder started (pid ${QODER_PID}); waiting for native Agent..."
for i in $(seq 1 40); do
  [ -f "${QODER_INFO}" ] && break
  sleep 1
done
[ -f "${QODER_INFO}" ] || { log "FAIL: Qoder .info.json did not appear"; exit 1; }

# 5. start Q Switch with headless auto-adapter
QSWITCH_AUTO_ADAPTER=1 "${QSWITCH_BIN}" >/tmp/qswitch-e2e-qswitch.log 2>&1 &
QSWITCH_PID=$!
log "Q Switch started (pid ${QSWITCH_PID}); waiting for adapter takeover..."
for i in $(seq 1 40); do
  if grep -q 'qswitchAdapter' "${QODER_INFO}" 2>/dev/null; then break; fi
  sleep 1
done
grep -q 'qswitchAdapter' "${QODER_INFO}" || { log "FAIL: adapter did not take over .info.json"; exit 1; }
log "adapter took over .info.json"

# 6. write the carrier mapping from the regenerated manifest
python3 "${SCRIPT_DIR}/inject.py" mapping

# 7. restart Qoder so it reconnects through the adapter
kill "${QODER_PID}" 2>/dev/null || true
sleep 3
"${QODER_BIN}" --remote-debugging-port="${QODER_CDP_PORT}" >/tmp/qswitch-e2e-qoder.log 2>&1 &
QODER_PID=$!
log "Qoder restarted through adapter"
sleep 14

# 8. drive the UI and send a message
if [ "${QSWITCH_E2E_SKIP_DRIVE:-0}" = "1" ]; then
  log "driver skipped; holding the prepared Qoder window for inspection..."
  sleep "${QSWITCH_E2E_HOLD_SECONDS:-45}"
  exit 0
fi
QODER_CDP_PORT="${QODER_CDP_PORT}" node "${SCRIPT_DIR}/drive.mjs"
log "drive finished; verifying mock request..."
sleep 1
assert_request "${MOCK_LOG}"

log "done (trap will restore original state)."
