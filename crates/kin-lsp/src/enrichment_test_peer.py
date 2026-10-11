# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

import json
import os
import sys

responses = json.loads(os.environ["KIN_LSP_TEST_RESPONSES"])
seen = []
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
    method = message["method"]
    closing_input = method == "test/close-input"
    if closing_input:
        os.close(0)
        reply = {"result": True}
    elif method == "test/seen":
        reply = {"result": seen}
    else:
        seen.append(message)
        if "id" not in message:
            continue
        params = message.get("params") or {}
        uri = params.get("textDocument", {}).get("uri", "")
        column = params.get("position", {}).get("character", "")
        reply = responses.get(f"{method}@{uri}#{column}", responses.get(method, {"result": None}))
    for notice in reply.get("before", []):
        body = json.dumps({"jsonrpc": "2.0", **notice}).encode()
        sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
        sys.stdout.buffer.flush()
    if reply.get("hold"):
        if reply.get("clock_hold"):
            # A controlled-clock test advances only after this process has
            # actually received the request it is timing out.
            notice = json.dumps({"jsonrpc": "2.0", "method": "$/progress", "params": {
                "token": message["id"], "value": {"kind": "begin", "title": "held request"}
            }}).encode()
            sys.stdout.buffer.write(f"Content-Length: {len(notice)}\r\n\r\n".encode() + notice)
            sys.stdout.buffer.flush()
        continue
    reply = {key: value for key, value in reply.items() if key != "before"}
    payload = json.dumps({"jsonrpc": "2.0", "id": message["id"], **reply}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload)
    sys.stdout.buffer.flush()
    if closing_input:
        import threading
        threading.Event().wait()
