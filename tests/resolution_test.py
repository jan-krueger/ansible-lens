#!/usr/bin/env python3
"""Host-context resolution: hovering in inv_demo/host_vars/web1 resolves
`demo_addr` the way web1 sees it — ordered by precedence, depth-aware, with the
other inventory's site hidden."""
import json
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(ROOT, "fixtures")
REPO = os.path.dirname(ROOT)
BIN = os.path.join(REPO, "target", "debug", "ansible-lens-lsp")
WEB1 = os.path.join(FIXTURES, "inv_demo", "host_vars", "web1.yml")


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


def until(p, i):
    while True:
        m = read(p)
        if m.get("id") == i and ("result" in m or "error" in m):
            return m


u = lambda path: "file://" + path


def check(name, cond):
    print(f"[{'PASS' if cond else 'FAIL'}] {name}")
    return cond


def main():
    p = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    send(p, {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "processId": os.getpid(), "rootUri": u(FIXTURES),
        "workspaceFolders": [{"uri": u(FIXTURES), "name": "fx"}], "capabilities": {}}})
    until(p, 1)
    send(p, {"jsonrpc": "2.0", "method": "initialized", "params": {}})
    time.sleep(0.8)

    text = open(WEB1).read()
    send(p, {"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
        "textDocument": {"uri": u(WEB1), "languageId": "yaml", "version": 1, "text": text}}})
    send(p, {"jsonrpc": "2.0", "id": 2, "method": "textDocument/hover", "params": {
        "textDocument": {"uri": u(WEB1)}, "position": {"line": 0, "character": 3}}})
    md = until(p, 2)["result"]["contents"]["value"]
    print(md)

    ok = True
    ok &= check("hover is host-resolved for web1 @ inv_demo",
                "resolved for `web1` @ `inv_demo`" in md)

    def pos(s):
        return md.find(s)

    # precedence order: host_vars > group_vars(webservers, depth 2) > group_vars(all) > role defaults
    order_ok = -1 < pos("host-level") < pos("web-level") < pos("all-level") < pos("role-default")
    ok &= check("applicable sites ordered host > group(deep) > group(all) > role default", order_ok)

    ok &= check("other inventory's site is hidden, not listed",
                "other-inventory" not in md and "don't apply" in md)
    ok &= check("runtime gap is disclosed",
                "runtime sources" in md)

    send(p, {"jsonrpc": "2.0", "id": 9, "method": "shutdown", "params": None})
    until(p, 9)
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
