#!/usr/bin/env bash
# Open a merchant check and print it as a QR code in the terminal — a stand-in
# for a merchant terminal while testing pay-by-QR from the customer app.
#
#   scripts/check-qr.sh <phone> <password> <amount, e.g. 12.50> [description] [base_url]
#
# Scan the QR with the app's "Pay QR" screen (or type the printed code). The
# script then polls the check and reports who paid it. Needs curl, jq, uuidgen
# and qrencode.
set -euo pipefail

phone=${1:?phone of the merchant account}
password=${2:?password}
amount=${3:?amount, e.g. 12.50}
description=${4:-}
base=${5:-http://localhost:8099}
json='content-type: application/json'

minor=$(python3 -c "from decimal import Decimal; print(int((Decimal('$amount') * 100).to_integral_value()))")
token=$(curl -sf -XPOST "$base/v1/auth/login" -H "$json" \
    -d "{\"phone\":\"$phone\",\"password\":\"$password\"}" | jq -r .access_token)
wallet=$(curl -sf "$base/v1/wallets" -H "Authorization: Bearer $token" | jq -r '.[0].id')
check_id=$(uuidgen)
check=$(curl -sf -XPOST "$base/v1/checks" -H "$json" -H "Authorization: Bearer $token" \
    -H "Idempotency-Key: $check_id" \
    -d "{\"account\":\"$wallet\",\"amount_minor\":$minor,\"description\":$(jq -Rn --arg d "$description" '$d | select(length > 0)') ,\"expires_in_secs\":900}" \
    || { echo "could not open the check (is the account KYC-verified?)" >&2; exit 1; })

echo "Check $(echo "$check" | jq -r .amount_minor | awk '{printf "%.2f", $1/100}') $(echo "$check" | jq -r .currency) — code: $check_id"
qrencode -t ANSIUTF8 "tjpay://check/$check_id"
echo "Waiting for a payment (15 min)…"

while :; do
    sleep 2
    status=$(curl -sf "$base/v1/checks/$check_id" -H "Authorization: Bearer $token" || echo '{"status":"?"}')
    case "$(echo "$status" | jq -r .status)" in
        open) ;;
        paid) echo "PAID by $(echo "$status" | jq -r '.payer_name // "an unverified user"') — transaction $(echo "$status" | jq -r .transaction_id)"; exit 0 ;;
        *) echo "Check is $(echo "$status" | jq -r .status)."; exit 1 ;;
    esac
done
