# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
#
# A scripted language server for the concurrency tests. It answers as
# enrichment_test_peer.py does, from `method@uri#column` or `method`, and in
# addition:
#
# - replies after a pseudo-random delay of up to `shuffle_ms`, from its own
#   thread, so answers arrive in a different order than the questions;
# - with `throttle` set, turns away with ServerCancelled any question that
#   arrives while that many are already waiting, the way a server under load
#   asks for a retry;
# - with `busy_at` set, answers a question that arrives while that many are
#   already waiting only after `busy_delay_ms`, the way a server under load
#   falls behind;
# - with `controlled_clock`, reports each reply's logical delay after receipt
#   and releases it only when the test clock reaches that delay;
# - records every `$/cancelRequest`, and the most questions it held at once.
#
# `test/seen` answers {"seen": [...], "cancelled": [...], "most_pending": n}.

import json
import os
import random
import sys
import threading

responses = json.loads(os.environ["KIN_LSP_TEST_RESPONSES"])
options = responses.pop("__options__", {})
shuffle_ms = options.get("shuffle_ms", 0)
throttle = options.get("throttle", 0)
busy_at = options.get("busy_at", 0)
busy_delay_ms = options.get("busy_delay_ms", 0)
controlled_clock = options.get("controlled_clock", False)
rng = random.Random(options.get("seed", 7))

lock = threading.Lock()
seen = []
cancelled = []
pending = set()
most_pending = [0]
clock_pending = {}


def write(message):
    payload = json.dumps({"jsonrpc": "2.0", **message}).encode()
    with lock:
        sys.stdout.buffer.write(f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload)
        sys.stdout.buffer.flush()


def answer_later(message_id, reply, delay):
    def fire():
        with lock:
            pending.discard(message_id)
        write({"id": message_id, **reply})

    timer = threading.Timer(delay, fire)
    timer.daemon = True
    timer.start()


while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\n", b"\r\n"):
            break
        name, value = line.decode().split(":", 1)
        headers[name.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers["content-length"])))
    method = message.get("method")
    params = message.get("params") or {}
    if method == "test/release-clock":
        for message_id in params["ids"]:
            with lock:
                reply = clock_pending.pop(message_id, None)
                pending.discard(message_id)
            if reply is not None:
                write({"id": message_id, **reply})
                write({"method": "$/progress", "params": {
                    "token": message_id, "value": {"kind": "end"}
                }})
        continue
    if method == "test/seen":
        with lock:
            result = {"seen": list(seen), "cancelled": list(cancelled), "most_pending": most_pending[0]}
        write({"id": message["id"], "result": result})
        continue
    if method == "$/cancelRequest":
        with lock:
            cancelled.append(params.get("id"))
            pending.discard(params.get("id"))
        continue
    with lock:
        seen.append(message)
    if "id" not in message:
        continue
    uri = params.get("textDocument", {}).get("uri", "")
    column = params.get("position", {}).get("character", "")
    reply = responses.get(f"{method}@{uri}#{column}", responses.get(method, {"result": None}))
    if controlled_clock and method == "shutdown":
        # The pass owns the controlled clock; teardown follows after it ends.
        write({"id": message["id"], **reply})
        continue
    with lock:
        waiting = len(pending)
    if throttle and waiting >= throttle:
        write({"id": message["id"], "error": {"code": -32802, "message": "server busy, retry"}})
        continue
    with lock:
        pending.add(message["id"])
        most_pending[0] = max(most_pending[0], len(pending))
    if reply.get("hold"):
        continue
    delay = rng.uniform(0, shuffle_ms) / 1000.0 if shuffle_ms else 0.0
    if busy_at and waiting >= busy_at:
        delay = busy_delay_ms / 1000.0
    if controlled_clock:
        with lock:
            clock_pending[message["id"]] = reply
        write({"method": "$/progress", "params": {
            "token": message["id"], "value": {"kind": "begin", "title": json.dumps({
                "clock_delay_ns": round(delay * 1_000_000_000)
            })}
        }})
        continue
    answer_later(message["id"], reply, delay)
