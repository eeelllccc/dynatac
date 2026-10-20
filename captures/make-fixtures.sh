#!/usr/bin/env bash
# Turn raw capture logs into golden fixtures for core/tests/golden_traces.rs.
#
#   ./captures/make-fixtures.sh
#
# Keeps only TRACE records, drops the capture tool's own diagnostics
# (heartbeats, slow-loop and key notes), and replaces personal data:
# the dialled number becomes 07700900123 (Ofcom drama range) and the IMEI
# becomes zeros. SMS PDUs carry numbers as swapped-nibble BCD hex, so those
# forms (national and international) are replaced too; the digit counts are
# equal, so the PDUs stay valid. The modem's bytes are otherwise untouched.
set -euo pipefail

cd "$(dirname "$0")/.."
OUT=core/tests/fixtures/modem
mkdir -p "$OUT"

# The real number lives in captures/.private-number (gitignored), never in
# a tracked file: captures/check-private.sh (the pre-commit hook) refuses
# commits containing it in any of the forms below.
REAL=$(tr -d '[:space:]' < captures/.private-number)
FAKE=07700900123

# Digits → SMS address BCD: nibbles swapped per octet, F-padded (`0792…` → `7029…`).
bcd() {
  local d=$1 out=""
  (( ${#d} % 2 )) && d="${d}F"
  for ((i = 0; i < ${#d}; i += 2)); do out+="${d:i+1:1}${d:i:1}"; done
  echo "$out"
}
SED_NUMBERS="s/${REAL}/${FAKE}/g; s/44${REAL:1}/44${FAKE:1}/g"
SED_NUMBERS+="; s/$(bcd "$REAL")/$(bcd "$FAKE")/g; s/$(bcd "44${REAL:1}")/$(bcd "44${FAKE:1}")/g"

# capture log → fixture name
FIXTURES=(
  "01-boot:01-boot"
  "02-out-answered-remote-hangup:02-out-answered-remote-hangup"
  "03-out-answered-local-hangup-3:03-out-answered-voicemail-hangup"
  "03-out-answered-local-hangup-4:03-out-answered-local-hangup"
  "04-out-rejected:04-out-rejected"
  "05-out-unanswered:05-out-unanswered"
  "06-out-busy-or-unreachable:06-out-busy-or-unreachable"
  "07-in-answered:07-in-answered"
  "08-in-missed:08-in-missed"
  "09-sms-during-call:09-sms-during-call"
  "10-soft-power-cycle:10-soft-power-cycle"
  "11-hard-power-cut:11-hard-power-cut"
  "12-command-during-urc:12-command-during-urc"
  "14-sms-receive-idle:14-sms-receive-idle"
  "15-sms-receive-in-call:15-sms-receive-in-call"
  "16-sms-send:16-sms-send"
)
# Not fixtures: 13-sms-state and the first runs of 14 (14-sms-received-idle,
# 14b-sms-delivery-check) list messages from other people. Their findings
# are in captures/README.md.

for pair in "${FIXTURES[@]}"; do
  src="captures/${pair%%:*}.log"
  dst="$OUT/${pair##*:}.trace"
  grep -E '^TRACE ' "$src" \
    | grep -vE '^TRACE [0-9]+ # (rx alive|alive |slow loop|key |kb error)' \
    | sed -E "${SED_NUMBERS}; s/IMEI: [0-9]{15}/IMEI: 000000000000000/" \
    > "$dst"
  echo "$dst: $(wc -l < "$dst") records"
done
