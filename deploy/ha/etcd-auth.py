"""Turn on etcd client authentication for the Patroni cluster (idempotent).

    etcd-auth.py

Creates the users `root` (secret etcd_root_password; etcd requires it before
auth can be enabled — keep it for emergencies) and `patroni` (secret
etcd_patroni_password), lets `patroni` read and write only Patroni's namespace
/service/, then enables authentication. Once enabled, every request without
valid credentials is refused, so nothing but Patroni can move the leader key.
Also creates `health` (password `health`, public on purpose, NO role): the
containers' healthcheck authenticates as it, because etcdctl reports a
cluster without credentials as unhealthy once auth is on, while a
permission-denied answer from a role-less user still proves consensus.
Re-runs make sure `health` exists and that `patroni` can still authenticate.

    HA_ETCD_HOSTS   comma-separated client endpoints (default etcd-1..3:2379)
    HA_SECRETS_DIR  secret files directory (default /run/secrets)

Talks to etcd's v3 JSON gateway with the standard library only.
"""

import base64
import json
import os
import sys
import time
import urllib.error
import urllib.request

NAMESPACE = "/service/"


def secret(name: str) -> str:
    with open(os.path.join(os.environ.get("HA_SECRETS_DIR", "/run/secrets"), name), encoding="utf-8") as f:
        value = f.read().strip()
    if len(value) < 16:
        sys.exit(f"etcd-auth: secret {name} is missing or shorter than 16 characters")
    return value


def b64(raw: bytes) -> str:
    return base64.b64encode(raw).decode()


class Etcd:
    def __init__(self, endpoints):
        self.endpoints = endpoints
        self.token = None

    def call(self, path: str, body: dict, ok_errors: tuple = ()) -> dict:
        """POST to the first endpoint that answers; an error whose message contains
        one of ok_errors counts as success (e.g. "already exists" on a re-run)."""
        last = None
        for endpoint in self.endpoints:
            req = urllib.request.Request(
                f"http://{endpoint}{path}", data=json.dumps(body).encode(), method="POST",
                headers={"Content-Type": "application/json", **({"Authorization": self.token} if self.token else {})},
            )
            try:
                with urllib.request.urlopen(req, timeout=5) as resp:
                    return json.load(resp)
            except urllib.error.HTTPError as e:
                detail = e.read().decode(errors="replace")
                if any(s in detail for s in ok_errors):
                    return {}
                sys.exit(f"etcd-auth: {path} on {endpoint}: HTTP {e.code} {detail}")
            except OSError as e:
                last = e
        raise ConnectionError(f"no etcd endpoint answered {path}: {last}")


def main() -> None:
    etcd = Etcd(os.environ.get("HA_ETCD_HOSTS", "etcd-1:2379,etcd-2:2379,etcd-3:2379").split(","))
    patroni_pw = secret("etcd_patroni_password")
    for attempt in range(60):
        try:
            # Once auth is on, even the status call wants a user: that refusal
            # is the answer. (A successful reply always carries a header; the
            # gateway omits enabled=false.)
            status = etcd.call("/v3/auth/status", {}, ("user name is empty",))
            enabled = not status or status.get("enabled", False)
            break
        except ConnectionError as e:
            if attempt == 59:
                sys.exit(f"etcd-auth: {e}")
            time.sleep(1)
    exists = ("already exists",)
    root_pw = secret("etcd_root_password")
    if enabled:
        etcd.token = etcd.call("/v3/auth/authenticate", {"name": "root", "password": root_pw})["token"]
        etcd.call("/v3/auth/user/add", {"name": "health", "password": "health"}, exists)
        etcd.token = None
        etcd.call("/v3/auth/authenticate", {"name": "patroni", "password": patroni_pw})
        print("etcd-auth: authentication already enabled; users health and patroni in place")
        return

    for user, password in (("root", root_pw), ("patroni", patroni_pw), ("health", "health")):
        etcd.call("/v3/auth/user/add", {"name": user, "password": password}, exists)
        etcd.call("/v3/auth/user/changepw", {"name": user, "password": password})
    for role in ("root", "patroni"):
        etcd.call("/v3/auth/role/add", {"name": role}, exists)
        etcd.call("/v3/auth/user/grant", {"user": role, "role": role})
    prefix = NAMESPACE.encode()
    etcd.call("/v3/auth/role/grant", {"name": "patroni", "perm": {
        "permType": "READWRITE", "key": b64(prefix), "range_end": b64(prefix[:-1] + bytes([prefix[-1] + 1])),
    }})
    etcd.call("/v3/auth/enable", {})
    etcd.call("/v3/auth/authenticate", {"name": "patroni", "password": patroni_pw})
    print(f"etcd-auth: authentication enabled; patroni may read/write {NAMESPACE} only, health nothing")


if __name__ == "__main__":
    main()
