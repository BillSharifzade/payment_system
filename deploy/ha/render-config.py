"""Render one node's effective Patroni configuration.

    render-config.py <shared patroni.yml> <output file>

Adds this node's identity and addresses (environment) and every credential
(secret files) to the shared deploy/ha/patroni.yml, so the shared file holds no
secret and patroni and patronictl read one 0600 file on a tmpfs:

    HA_NODE_NAME            member name (required), e.g. pg-1
    HA_PG_CONNECT           host:port other members and HAProxy reach Postgres at (default <name>:5432)
    HA_RESTAPI_CONNECT      host:port of this node's REST API (default <name>:8008)
    HA_PG_LISTEN, HA_RESTAPI_LISTEN, HA_PG_DATA_DIR   optional overrides
    HA_ETCD_HOSTS           comma-separated etcd client endpoints (default etcd-1..3:2379)
    HA_SECRETS_DIR          where the secret files are (default /run/secrets):
                            postgres_password, pg_replication_password,
                            patroni_api_password, etcd_patroni_password
"""

import os
import sys

import yaml


def secret(directory: str, name: str) -> str:
    with open(os.path.join(directory, name), encoding="utf-8") as f:
        value = f.read().strip()
    if len(value) < 16:
        sys.exit(f"render-config: secret {name} is missing or shorter than 16 characters")
    return value


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    src, dst = sys.argv[1:]
    env = os.environ
    name = env.get("HA_NODE_NAME") or sys.exit("render-config: HA_NODE_NAME is required")
    secrets = env.get("HA_SECRETS_DIR", "/run/secrets")
    with open(src, encoding="utf-8") as f:
        cfg = yaml.safe_load(f)

    api = {"username": "patroni", "password": secret(secrets, "patroni_api_password")}
    cfg["name"] = name
    cfg["restapi"].update(
        connect_address=env.get("HA_RESTAPI_CONNECT", f"{name}:8008"),
        authentication=api,
    )
    if "HA_RESTAPI_LISTEN" in env:
        cfg["restapi"]["listen"] = env["HA_RESTAPI_LISTEN"]
    # patronictl (switchover, edit-config, ...) authenticates with the same user.
    cfg["ctl"] = {"authentication": api}
    cfg["etcd3"].update(
        hosts=env.get("HA_ETCD_HOSTS", "etcd-1:2379,etcd-2:2379,etcd-3:2379").split(","),
        username="patroni",
        password=secret(secrets, "etcd_patroni_password"),
    )
    pg = cfg["postgresql"]
    pg["connect_address"] = env.get("HA_PG_CONNECT", f"{name}:5432")
    if "HA_PG_LISTEN" in env:
        pg["listen"] = env["HA_PG_LISTEN"]
    if "HA_PG_DATA_DIR" in env:
        pg["data_dir"] = env["HA_PG_DATA_DIR"]
    pg["authentication"] = {
        "superuser": {"username": "payment", "password": secret(secrets, "postgres_password")},
        "replication": {"username": "replicator", "password": secret(secrets, "pg_replication_password")},
    }

    fd = os.open(dst, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    os.fchmod(fd, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        yaml.safe_dump(cfg, f, default_flow_style=False, sort_keys=False)


if __name__ == "__main__":
    main()
