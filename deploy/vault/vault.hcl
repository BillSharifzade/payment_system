# Vault server for docker-compose.vault.yml: one node, integrated (Raft)
# storage on the vaultdata volume. It holds the checkpoint signing key in the
# transit engine; payment-workers reaches it over the dedicated, internal
# `vault` network, which no other service joins.
#
# Plain HTTP on that network, like Postgres/NATS on `backend`: only a signing
# request (a hash) and the AppRole login cross it. Across hosts, enable TLS
# here (tls_cert_file/tls_key_file) and give the workers VAULT_CACERT.

ui            = false
# Containers cannot mlock without IPC_LOCK; keep swap off on the host instead.
disable_mlock = true

storage "raft" {
  path    = "/vault/file"
  node_id = "vault-1"
}

listener "tcp" {
  address     = "0.0.0.0:8200"
  tls_disable = true
}

api_addr     = "http://vault:8200"
cluster_addr = "http://vault:8201"
