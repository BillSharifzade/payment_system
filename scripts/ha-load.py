#!/usr/bin/env python3
"""Transfer load for scripts/ha-failover-drill.sh, and the analysis of its log.

    ha-load.py run --api URL --users users.json --out DIR [--rps 40] [--threads 8]
        Posts POST /v1/transfers between the users' wallets until SIGTERM (then
        finishes the requests in flight and retries pending keys for up to
        --grace seconds). A request that fails without a definite answer
        (connection error, timeout, 5xx) is retried with the SAME Idempotency-Key
        — the client behaviour the API's idempotency contract expects — so an
        ambiguous commit resolves to "posted" or "already_posted", never to a
        double spend. Writes DIR/attempts.tsv (every attempt) and DIR/acked.txt
        (the transaction id of every 2xx answer: what the client was told is
        durable).
    ha-load.py analyze --out DIR --fault-ms T
        JSON metrics of the client's view around a fault injected at T.

users.json: [{"phone": ..., "password": ..., "wallet": ...}, ...] (registered,
KYC'd and funded by the drill). Standard library only.
"""

import argparse
import http.client
import json
import os
import random
import signal
import sys
import threading
import time
import uuid
from urllib.parse import urlsplit


def now_ms() -> int:
    return time.time_ns() // 1_000_000


class Client:
    """One keep-alive HTTP connection per thread; reconnects after any failure."""

    def __init__(self, api: str, timeout: float):
        u = urlsplit(api)
        self.host, self.port, self.timeout = u.hostname, u.port or 80, timeout
        self.conn = None

    def request(self, method: str, path: str, body=None, headers=None):
        if self.conn is None:
            self.conn = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)
        try:
            self.conn.request(method, path, body=json.dumps(body) if body is not None else None,
                              headers={"content-type": "application/json", **(headers or {})})
            resp = self.conn.getresponse()
            data = resp.read()
            return resp.status, data
        except (OSError, http.client.HTTPException) as e:
            self.conn.close()
            self.conn = None
            return None, type(e).__name__.encode()


class Load:
    def __init__(self, args):
        self.args = args
        with open(args.users, encoding="utf-8") as f:
            self.users = json.load(f)
        self.tokens = {}
        self.tokens_lock = threading.Lock()
        self.stop = threading.Event()
        self.deadline = None  # set on stop: pending keys are retried until then
        self.interval = args.threads / args.rps
        self.log_lock = threading.Lock()
        os.makedirs(args.out, exist_ok=True)
        self.attempts = open(os.path.join(args.out, "attempts.tsv"), "a", buffering=1, encoding="utf-8")
        self.acked = open(os.path.join(args.out, "acked.txt"), "a", buffering=1, encoding="utf-8")

    def token(self, client: Client, user: dict, refresh: bool = False):
        with self.tokens_lock:
            if not refresh and user["phone"] in self.tokens:
                return self.tokens[user["phone"]]
        status, data = client.request("POST", "/v1/auth/login",
                                      {"phone": user["phone"], "password": user["password"]})
        if status != 200:
            return None
        tok = json.loads(data)["access_token"]
        with self.tokens_lock:
            self.tokens[user["phone"]] = tok
        return tok

    def record(self, start: int, end: int, key: str, attempt: int, outcome: str):
        with self.log_lock:
            self.attempts.write(f"{start}\t{end}\t{key}\t{attempt}\t{outcome}\n")

    def transfer(self, client: Client, rng: random.Random):
        sender, receiver = rng.sample(self.users, 2)
        key = str(uuid.uuid4())
        body = {"from_account": sender["wallet"], "to_account": receiver["wallet"],
                "amount_minor": rng.randint(1, 100), "currency": "TJS"}
        attempt = 0
        while True:
            attempt += 1
            if self.stop.is_set() and time.time() > self.deadline:
                self.record(now_ms(), now_ms(), key, attempt, "abandoned")
                return
            tok = self.token(client, sender)
            start = now_ms()
            if tok is None:
                status, data = None, b"login_failed"
            else:
                status, data = client.request("POST", "/v1/transfers", body,
                                              {"authorization": f"Bearer {tok}", "idempotency-key": key})
            end = now_ms()
            outcome = str(status) if status is not None else f"ERR:{data.decode()}"
            self.record(start, end, key, attempt, outcome)
            if status in (200, 201):
                with self.log_lock:
                    self.acked.write(f"{key}\t{end}\n")
                return
            if status == 401:
                self.token(client, sender, refresh=True)
            elif status is not None and 400 <= status < 500 and status not in (408, 429):
                return  # a definite refusal: nothing was posted
            time.sleep(min(0.05 * attempt, 0.5))

    def worker(self, seed: int):
        client = Client(self.args.api, self.args.timeout)
        rng = random.Random(seed)
        next_at = time.time() + rng.random() * self.interval
        while not self.stop.is_set():
            delay = next_at - time.time()
            if delay > 0:
                self.stop.wait(delay)
                if self.stop.is_set():
                    break
            next_at = max(next_at + self.interval, time.time() - self.interval)
            self.transfer(client, rng)

    def run(self):
        def on_term(*_):
            self.deadline = time.time() + self.args.grace
            self.stop.set()
        signal.signal(signal.SIGTERM, on_term)
        signal.signal(signal.SIGINT, on_term)
        threads = [threading.Thread(target=self.worker, args=(i,), daemon=True) for i in range(self.args.threads)]
        for t in threads:
            t.start()
        while any(t.is_alive() for t in threads):
            for t in threads:
                t.join(0.2)


def analyze(out: str, fault_ms: int) -> dict:
    rows = []
    with open(os.path.join(out, "attempts.tsv"), encoding="utf-8") as f:
        for line in f:
            start, end, key, attempt, outcome = line.rstrip("\n").split("\t")
            rows.append((int(start), int(end), key, int(attempt), outcome))
    ok = sorted(end for _, end, _, _, o in rows if o in ("200", "201"))
    after = [r for r in rows if r[1] >= fault_ms]
    errors = [r for r in after if r[4] not in ("200", "201")]
    statuses = {}
    for r in after:
        statuses[r[4]] = statuses.get(r[4], 0) + 1
    # Longest stretch without a single acknowledged transfer that covers the fault.
    before = [t for t in ok if t <= fault_ms]
    acks = ([before[-1]] if before else [fault_ms]) + [t for t in ok if t > fault_ms]
    gap = max((b - a for a, b in zip(acks, acks[1:])), default=None)
    first_ok_after_errors = next((t for t in ok if errors and t > errors[-1][1]), None)
    return {
        "attempts_total": len(rows),
        "acked_total": len(ok),
        "attempts_after_fault": len(after),
        "errors_after_fault": len(errors),
        "statuses_after_fault": statuses,
        "error_window_s": round((errors[-1][1] - errors[0][0]) / 1000, 2) if errors else 0.0,
        "first_error_after_fault_s": round((errors[0][1] - fault_ms) / 1000, 2) if errors else None,
        "write_outage_s": round(gap / 1000, 2) if gap is not None else None,
        "recovered_after_fault_s": round((first_ok_after_errors - fault_ms) / 1000, 2) if first_ok_after_errors else None,
        "acked_after_retry": len({r[2] for r in rows if r[3] > 1 and r[4] in ("200", "201")}),
        "abandoned": sum(1 for r in rows if r[4] == "abandoned"),
        "max_ack_latency_s": round(max((r[1] - r[0] for r in rows if r[4] in ("200", "201")), default=0) / 1000, 2),
    }


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--api", required=True)
    r.add_argument("--users", required=True)
    r.add_argument("--out", required=True)
    r.add_argument("--rps", type=float, default=40.0)
    r.add_argument("--threads", type=int, default=8)
    r.add_argument("--timeout", type=float, default=15.0)
    r.add_argument("--grace", type=float, default=30.0)
    a = sub.add_parser("analyze")
    a.add_argument("--out", required=True)
    a.add_argument("--fault-ms", type=int, required=True)
    args = p.parse_args()
    if args.cmd == "run":
        if args.rps <= 0 or args.threads <= 0:
            sys.exit("--rps and --threads must be positive")
        Load(args).run()
    else:
        print(json.dumps(analyze(args.out, args.fault_ms)))


if __name__ == "__main__":
    main()
