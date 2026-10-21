#!/usr/bin/env bash
# One-time Vault setup for checkpoint signing, run with an admin token after
# `vault operator init` + unseal (see docker-compose.vault.yml):
#   1. the transit engine and a NON-exportable ed25519 key (its private half
#      never leaves Vault — not even in a backup);
#   2. the sign-only policy (checkpoint-signer.hcl);
#   3. an AppRole bound to that policy alone (no default policy), whose
#      role_id/secret_id are written to <secrets-dir>/vault_role_id and
#      vault_secret_id for payment-workers;
#   4. prints the key's public half: add it to worker_trusted_public_keys
#      BEFORE switching the workers to WORKER_SIGNER=vault.
#
#   VAULT_ADDR=http://127.0.0.1:8200 VAULT_TOKEN=<admin> deploy/vault/bootstrap.sh [secrets-dir]
#
# Without a local vault CLI, through the overlay's container (run from deploy/):
#   VAULT_TOKEN=<admin> VAULT_CMD="docker compose exec -T -e VAULT_TOKEN vault vault" vault/bootstrap.sh
#
# Environment: VAULT_TRANSIT_KEY (checkpoint-signing), VAULT_APPROLE_ROLE
# (payment-workers), VAULT_BOUND_CIDRS (optional, e.g. the vault network's
# subnet: the secret_id and its tokens only work from there), VAULT_CMD (vault).
# Re-running is safe: the key is kept, policy and role are re-applied, and a
# NEW secret_id is issued (destroy the old one after redeploying:
# vault list auth/approle/role/<role>/secret-id; ... secret-id-accessor/destroy).
# Needs jq.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
secrets=${1:-$here/../secrets}
key=${VAULT_TRANSIT_KEY:-checkpoint-signing}
role=${VAULT_APPROLE_ROLE:-payment-workers}
read -r -a vault_cmd <<<"${VAULT_CMD:-vault}"
v() { "${vault_cmd[@]}" "$@"; }
die() { echo "bootstrap: $*" >&2; exit 1; }

: "${VAULT_TOKEN:?VAULT_TOKEN must hold an admin token}"
[[ "$key" =~ ^[A-Za-z0-9_.-]+$ ]] || die "bad VAULT_TRANSIT_KEY: $key"
[[ "$role" =~ ^[A-Za-z0-9_.-]+$ ]] || die "bad VAULT_APPROLE_ROLE: $role"
command -v jq >/dev/null || die "jq is required"

if ! v secrets list -format=json | jq -e 'has("transit/")' >/dev/null; then
  v secrets enable transit
fi
if ! v read -format=json "transit/keys/$key" >/dev/null 2>&1; then
  v write -f "transit/keys/$key" type=ed25519 exportable=false allow_plaintext_backup=false >/dev/null
  echo "created transit key $key (ed25519, non-exportable)"
fi
info=$(v read -format=json "transit/keys/$key")
jq -e '.data.type == "ed25519" and .data.exportable == false and .data.allow_plaintext_backup == false' \
  <<<"$info" >/dev/null \
  || die "transit/keys/$key exists but is not a non-exportable ed25519 key; pick another VAULT_TRANSIT_KEY"

sed "s/checkpoint-signing/$key/g" "$here/checkpoint-signer.hcl" | v policy write checkpoint-signer - >/dev/null
if ! v auth list -format=json | jq -e 'has("approle/")' >/dev/null; then
  v auth enable approle
fi
bound=()
if [[ -n "${VAULT_BOUND_CIDRS:-}" ]]; then
  bound=("secret_id_bound_cidrs=$VAULT_BOUND_CIDRS" "token_bound_cidrs=$VAULT_BOUND_CIDRS")
fi
# Short-lived tokens; the worker logs in again before they lapse.
v write "auth/approle/role/$role" \
  token_policies=checkpoint-signer token_no_default_policy=true \
  token_ttl=1h token_max_ttl=4h token_num_uses=0 \
  secret_id_ttl=0 secret_id_num_uses=0 "${bound[@]}" >/dev/null

role_id=$(v read -field=role_id "auth/approle/role/$role/role-id")
secret_id=$(v write -f -field=secret_id "auth/approle/role/$role/secret-id")
mkdir -p "$secrets"
(umask 077
 printf '%s\n' "$role_id" > "$secrets/vault_role_id"
 printf '%s\n' "$secret_id" > "$secrets/vault_secret_id")

public_key=$(jq -r '.data.keys[(.data.latest_version | tostring)].public_key' <<<"$info" \
  | base64 -d | od -An -tx1 -v | tr -d ' \n')
[[ "$public_key" =~ ^[0-9a-f]{64}$ ]] || die "could not read the key's public half"
echo "AppRole $role: credentials written to $secrets/vault_role_id and vault_secret_id"
echo "checkpoint public key: $public_key"
echo "next: append it to secrets/worker_trusted_public_keys (comma-separated, keep the old keys),"
echo "      then deploy with COMPOSE_FILE=...:docker-compose.vault.yml"
