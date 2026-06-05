#!/usr/bin/env python3
"""Verify undefined-variable diagnostics: a {{ usage }} with no definition is
flagged, while defined vars, magic/dynamic vars, and filters are not."""
import json
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(ROOT, "fixtures")
REPO = os.path.dirname(ROOT)
BIN = os.path.join(REPO, "target", "debug", "ansible-lens-lsp")
DOC = os.path.join(FIXTURES, "diag_scratch.yml")

# line 0 defined (percona), line 1 undefined, lines 2-4 must NOT flag
TEXT = (
    'a: "{{ percona.administration.password }}"\n'
    'b: "{{ totally_undefined_xyz }}"\n'
    'c: "{{ item.name }}"\n'
    'd: "{{ ansible_hostname }}"\n'
    'e: "{{ percona.administration.password | default(\'x\') }}"\n'
)


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
    n = int(dict(l.split(b": ") for l in h.strip().split(b"\r\n"))[b"Content-Length"])
    return json.loads(p.stdout.read(n))


u = lambda path: "file://" + path


def main():
    p = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    send(p, {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "processId": os.getpid(), "rootUri": u(FIXTURES),
        "workspaceFolders": [{"uri": u(FIXTURES), "name": "fx"}], "capabilities": {}}})
    while read(p).get("id") != 1:
        pass
    send(p, {"jsonrpc": "2.0", "method": "initialized", "params": {}})
    time.sleep(0.6)

    send(p, {"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
        "textDocument": {"uri": u(DOC), "languageId": "yaml", "version": 1, "text": TEXT}}})

    # wait for the publishDiagnostics notification for our document
    diags = None
    deadline = 0
    while diags is None and deadline < 200:
        m = read(p)
        if m.get("method") == "textDocument/publishDiagnostics" and m["params"]["uri"] == u(DOC):
            diags = m["params"]["diagnostics"]
        deadline += 1

    ok = True
    flagged = sorted((d["range"]["start"]["line"], d["message"]) for d in (diags or []))
    print("diagnostics:")
    for ln, msg in flagged:
        print(f"  line {ln}: {msg}")

    ok &= _check("exactly one diagnostic", len(diags or []) == 1)
    ok &= _check("flag is on line 1 (the undefined var)",
                 bool(diags) and diags[0]["range"]["start"]["line"] == 1)
    ok &= _check("message names the undefined root",
                 bool(diags) and "totally_undefined_xyz" in diags[0]["message"])
    ok &= _check("defined / magic / filter vars are NOT flagged",
                 all("totally_undefined_xyz" in d["message"] for d in (diags or [])))

    send(p, {"jsonrpc": "2.0", "id": 9, "method": "shutdown", "params": None})
    while read(p).get("id") != 9:
        pass
    send(p, {"jsonrpc": "2.0", "method": "exit", "params": None})
    p.stdin.close()
    try:
        p.wait(timeout=2)
    except subprocess.TimeoutExpired:
        p.kill()

    print("\n" + ("ALL PASS" if ok else "SOME FAILED"))
    sys.exit(0 if ok else 1)


def _check(name, cond):
    print(f"[{'PASS' if cond else 'FAIL'}] {name}")
    return cond


if __name__ == "__main__":
    main()
