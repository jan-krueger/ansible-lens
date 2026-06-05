#!/usr/bin/env python3
"""Verify workspace/didChangeWatchedFiles: a file created/changed/deleted on
disk (not via an open buffer) updates the index."""

import json
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(ROOT, "fixtures")
REPO = os.path.dirname(ROOT)
BIN = os.path.join(REPO, "target", "debug", "ansible-lens-lsp")
NEWFILE = os.path.join(FIXTURES, "group_vars", "watched.yml")


def frame(o):
    b = json.dumps(o).encode()
    return b"Content-Length: %d\r\n\r\n%s" % (len(b), b)


def send(p, o):
    p.stdin.write(frame(o))
    p.stdin.flush()


def read(p):
    h = b""
    while b"\r\n\r\n" not in h:
        h += p.stdout.read(1)
    n = int(dict(ln.split(b": ") for ln in h.strip().split(b"\r\n"))[b"Content-Length"])
    return json.loads(p.stdout.read(n))


SAW_REGISTER = [False]


def pump_until_id(p, want_id):
    """Read messages until our response arrives, answering any server→client
    requests (e.g. client/registerCapability) along the way."""
    while True:
        m = read(p)
        if m.get("method") == "client/registerCapability" and "id" in m:
            SAW_REGISTER[0] = True
            send(p, {"jsonrpc": "2.0", "id": m["id"], "result": None})
            continue
        if m.get("id") == want_id and ("result" in m or "error" in m):
            return m


def u(path):
    return "file://" + path


_id = [100]


def completion_labels(p, path, line, character):
    _id[0] += 1
    rid = _id[0]
    send(
        p,
        {
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": u(path),
                    "languageId": "yaml",
                    "version": 1,
                    "text": open(path).read(),
                }
            },
        },
    )
    send(
        p,
        {
            "jsonrpc": "2.0",
            "id": rid,
            "method": "textDocument/completion",
            "params": {
                "textDocument": {"uri": u(path)},
                "position": {"line": line, "character": character},
            },
        },
    )
    res = pump_until_id(p, rid)
    items = res["result"]
    items = items["items"] if isinstance(items, dict) else (items or [])
    send(
        p,
        {
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": {"textDocument": {"uri": u(path)}},
        },
    )
    return [i["label"] for i in items]


def watched(p, path, typ):
    # 1=Created, 2=Changed, 3=Deleted
    send(
        p,
        {
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWatchedFiles",
            "params": {"changes": [{"uri": u(path), "type": typ}]},
        },
    )


def main():
    if os.path.exists(NEWFILE):
        os.remove(NEWFILE)
    p = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    send(
        p,
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": os.getpid(),
                "rootUri": u(FIXTURES),
                "workspaceFolders": [{"uri": u(FIXTURES), "name": "fx"}],
                "capabilities": {
                    "workspace": {
                        "didChangeWatchedFiles": {"dynamicRegistration": True}
                    }
                },
            },
        },
    )
    pump_until_id(p, 1)
    send(p, {"jsonrpc": "2.0", "method": "initialized", "params": {}})
    time.sleep(0.6)

    # a scratch buffer to complete top-level vars in
    scratch = os.path.join(FIXTURES, "watch_scratch.yml")
    open(scratch, "w").write('m: "{{  }}"\n')
    col = 'm: "{{ '.index("{{ ") + 3

    ok = True
    try:
        # the first round-trip also lets us observe the registerCapability request
        before = completion_labels(p, scratch, 0, col)
        print(
            f"[{'PASS' if SAW_REGISTER[0] else 'FAIL'}] server registered file watchers"
        )
        ok &= SAW_REGISTER[0]

        print(
            f"[{'PASS' if 'watched_thing' not in before else 'FAIL'}] var absent before file exists"
        )
        ok &= "watched_thing" not in before

        # create the file on disk + notify (Created=1)
        open(NEWFILE, "w").write("watched_thing:\n  child: 1\n")
        watched(p, NEWFILE, 1)
        time.sleep(0.2)
        after = completion_labels(p, scratch, 0, col)
        print(
            f"[{'PASS' if 'watched_thing' in after else 'FAIL'}] var appears after Created event"
        )
        ok &= "watched_thing" in after

        # delete the file + notify (Deleted=3)
        os.remove(NEWFILE)
        watched(p, NEWFILE, 3)
        time.sleep(0.2)
        gone = completion_labels(p, scratch, 0, col)
        print(
            f"[{'PASS' if 'watched_thing' not in gone else 'FAIL'}] var removed after Deleted event"
        )
        ok &= "watched_thing" not in gone
    finally:
        for f in (scratch, NEWFILE):
            if os.path.exists(f):
                os.remove(f)

    send(p, {"jsonrpc": "2.0", "id": 999, "method": "shutdown", "params": None})
    pump_until_id(p, 999)
    send(p, {"jsonrpc": "2.0", "method": "exit", "params": None})
    p.stdin.close()
    try:
        p.wait(timeout=2)
    except subprocess.TimeoutExpired:
        p.kill()

    print("\n" + ("ALL PASS" if ok else "SOME FAILED"))
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
