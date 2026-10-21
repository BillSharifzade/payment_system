# Policy of the payment-workers AppRole: sign with the checkpoint key and read
# its public half — nothing else. No export, rotate, config, other keys, and
# (the role sets token_no_default_policy) not even the default policy.
# bootstrap.sh substitutes the key name when VAULT_TRANSIT_KEY is not the
# default.

path "transit/sign/checkpoint-signing" {
  capabilities = ["update"]
  # What the worker sends; anything else (batch_input, prehashed, context,
  # a different hash) is refused by Vault before the transit engine sees it.
  allowed_parameters = {
    "input"       = []
    "key_version" = []
  }
}

path "transit/keys/checkpoint-signing" {
  capabilities = ["read"]
}
