#!/usr/bin/env python3
"""Drive the installed server against a real workspace + file to see whether the
*server logic* resolves a variable, independent of Zed's attachment."""
import json, os, subprocess, sys, time

BIN = os.path.expanduser("~/.cargo/bin/ansible-lens-lsp")
ROOT = sys.argv[1]
FILE = sys.argv[2]
NEEDLE = sys.argv[3]            # substring to place the cursor on
LINE_SUBSTR = sys.argv[4]      # a substring identifying the line


def frame(o):
    b = json.dumps(o).encode()
    return b"Content-Length: %d\r\n\r\n%s" % (len(b), b)


def send(p, o):
    p.stdin.write(frame(o)); p.stdin.flush()


def read(p):
    h = b""
    while b"\r\n\r\n" not in h:
        h += p.stdout.read(1)
    n = int(dict(l.split(b": ") for l in h.strip().split(b"\r\n"))[b"Content-Length"])
    return json.loads(p.stdout.read(n))


def until(p, i):
    while True:
        m = read(p)
        if m.get("method") == "window/logMessage":
            print("  [server log]", m["params"]["message"])
        if m.get("id") == i and ("result" in m or "error" in m):
            return m


u = lambda path: "file://" + path
p = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
send(p, {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "processId": os.getpid(), "rootUri": u(ROOT),
    "workspaceFolders": [{"uri": u(ROOT), "name": "root"}], "capabilities": {}}})
until(p, 1)
send(p, {"jsonrpc": "2.0", "method": "initialized", "params": {}})
time.sleep(1.2)

lines = open(FILE).read().splitlines()
lineno = next(i for i, l in enumerate(lines) if LINE_SUBSTR in l)
col = lines[lineno].find(NEEDLE) + len(NEEDLE) // 2
print(f"querying {os.path.basename(FILE)}:{lineno} col {col}  -> '{lines[lineno].strip()}'")

REFS = len(sys.argv) > 5 and sys.argv[5] == "ref"
if REFS:
    send(p, {"jsonrpc": "2.0", "id": 2, "method": "textDocument/references", "params": {
        "textDocument": {"uri": u(FILE)}, "position": {"line": lineno, "character": col},
        "context": {"includeDeclaration": False}}})
else:
    send(p, {"jsonrpc": "2.0", "id": 2, "method": "textDocument/definition", "params": {
        "textDocument": {"uri": u(FILE)}, "position": {"line": lineno, "character": col}}})
res = until(p, 2)["result"] or []
print(f"\n{len(res)} {'reference' if REFS else 'definition'}(s):")
for loc in res:
    r = loc["range"]["start"]
    print(f"  {loc['uri'].replace('file://'+ROOT+'/', '')}  (line {r['line']}, col {r['character']})")

send(p, {"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": None}); until(p, 3)
send(p, {"jsonrpc": "2.0", "method": "exit", "params": None}); p.stdin.close()
try: p.wait(timeout=2)
except subprocess.TimeoutExpired: p.kill()
