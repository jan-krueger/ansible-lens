#!/usr/bin/env python3
"""End-to-end LSP smoke test: drive the server over stdio against the fixtures
and verify Go-to-Definition for exact paths, precedence ordering, index
variables, and glob (post-dynamic-index) suffixes."""
import json
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(ROOT, "fixtures")
REPO = os.path.dirname(ROOT)
BIN = os.path.join(REPO, "target", "debug", "ansible-lens-lsp")
PLAYBOOK = os.path.join(FIXTURES, "playbook.yml")
LINES = open(PLAYBOOK).read().splitlines()


def frame(obj):
    body = json.dumps(obj).encode()
    return b"Content-Length: %d\r\n\r\n%s" % (len(body), body)


def send(proc, obj):
    proc.stdin.write(frame(obj))
    proc.stdin.flush()


def read_message(proc):
    headers = b""
    while b"\r\n\r\n" not in headers:
        headers += proc.stdout.read(1)
    length = int(dict(
        line.split(b": ") for line in headers.strip().split(b"\r\n")
    )[b"Content-Length"])
    return json.loads(proc.stdout.read(length))


def read_until_id(proc, want_id):
    while True:
        msg = read_message(proc)
        if msg.get("id") == want_id and ("result" in msg or "error" in msg):
            return msg


def uri(path):
    return "file://" + path


_next_id = [10]


def definition(proc, line, character):
    _next_id[0] += 1
    rid = _next_id[0]
    send(proc, {
        "jsonrpc": "2.0", "id": rid, "method": "textDocument/definition",
        "params": {
            "textDocument": {"uri": uri(PLAYBOOK)},
            "position": {"line": line, "character": character},
        },
    })
    locations = read_until_id(proc, rid)["result"] or []
    return [loc["uri"].split("/fixtures/")[-1] for loc in locations]


def references(proc, path, line, character, include_decl=False):
    _next_id[0] += 1
    rid = _next_id[0]
    send(proc, {
        "jsonrpc": "2.0", "id": rid, "method": "textDocument/references",
        "params": {
            "textDocument": {"uri": uri(path)},
            "position": {"line": line, "character": character},
            "context": {"includeDeclaration": include_decl},
        },
    })
    locs = read_until_id(proc, rid)["result"] or []
    return sorted(
        f"{loc['uri'].split('/fixtures/')[-1]}:{loc['range']['start']['line']}"
        for loc in locs
    )


def check(name, got, expected):
    status = "PASS" if got == expected else "FAIL"
    print(f"[{status}] {name}")
    if got != expected:
        print(f"        expected: {expected}")
        print(f"        got:      {got}")
    return got == expected


def main():
    proc = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    send(proc, {
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "processId": os.getpid(),
            "rootUri": uri(FIXTURES),
            "workspaceFolders": [{"uri": uri(FIXTURES), "name": "fixtures"}],
            "capabilities": {},
        },
    })
    read_until_id(proc, 1)
    send(proc, {"jsonrpc": "2.0", "method": "initialized", "params": {}})
    time.sleep(0.6)  # let the background index build

    ok = True

    # 1. exact nested path -> all 5 tiers, precedence-ordered (winner first)
    line = LINES.index('        msg: "{{ percona.administration.password }}"')
    col = LINES[line].rfind("password") + 3
    ok &= check(
        "exact path returns 5 defs in precedence order",
        definition(proc, line, col),
        [
            "roles/percona/vars/main.yml",
            "host_vars/db1.yml",
            "group_vars/db_servers.yml",
            "group_vars/all.yml",
            "roles/percona/defaults/main.yml",
        ],
    )

    # console line: vault.cppd[cppd_environment].console_api_key
    cline = next(i for i, l in enumerate(LINES) if "cppd_environment" in l)
    text = LINES[cline]

    # 2. click the index variable -> resolves to its own definition
    ok &= check(
        "index var 'cppd_environment' resolves to itself",
        definition(proc, cline, text.find("cppd_environment") + 3),
        ["group_vars/vault.yml"],
    )

    # 3. click the prefix before the index -> the concrete dict
    ok &= check(
        "prefix before index resolves to vault.cppd dict",
        definition(proc, cline, text.find("cppd[")),
        ["group_vars/vault.yml"],
    )

    # 4. click the suffix after the index -> glob over all environments
    ok &= check(
        "suffix after index globs all 3 environments",
        sorted(definition(proc, cline, text.find("console_api_key") + 3)),
        ["group_vars/vault.yml", "group_vars/vault.yml", "group_vars/vault.yml"],
    )

    # --- references (definition -> usages) ----------------------------------
    defaults = os.path.join(FIXTURES, "roles/percona/defaults/main.yml")
    vault = os.path.join(FIXTURES, "group_vars/vault.yml")

    # 5. from the `password` definition key -> its usage in the playbook
    ok &= check(
        "references: password def -> playbook usage",
        references(proc, defaults, 2, 4),
        ["playbook.yml:4"],
    )

    # 6. includeDeclaration adds the definition sites (all precedence tiers)
    ok &= check(
        "references: includeDeclaration adds all def sites",
        references(proc, defaults, 2, 4, include_decl=True),
        [
            "group_vars/all.yml:2",
            "group_vars/db_servers.yml:2",
            "host_vars/db1.yml:2",
            "playbook.yml:4",
            "roles/percona/defaults/main.yml:2",
            "roles/percona/vars/main.yml:2",
        ],
    )

    # 7. reverse-glob: concrete def `vault.cppd.prod.console_api_key`
    #    matches the dynamic-index usage `vault.cppd[cppd_environment]....`
    ok &= check(
        "references: concrete def -> dynamic-index usage",
        references(proc, vault, 4, 6),
        ["playbook.yml:7"],
    )

    # 8. index variable: `cppd_environment` def -> its use as an index
    ok &= check(
        "references: index var def -> index usage",
        references(proc, vault, 0, 0),
        ["playbook.yml:7"],
    )

    # --- completion (inside {{ }}) ------------------------------------------
    def completion(path, line, character):
        _next_id[0] += 1
        rid = _next_id[0]
        send(proc, {
            "jsonrpc": "2.0", "id": rid, "method": "textDocument/completion",
            "params": {
                "textDocument": {"uri": uri(path)},
                "position": {"line": line, "character": character},
            },
        })
        res = read_until_id(proc, rid)["result"] or []
        items = res["items"] if isinstance(res, dict) else res
        return sorted(i["label"] for i in items)

    # a scratch file referencing vars; cursor right after `{{ percona.`
    scratch = os.path.join(FIXTURES, "scratch_complete.yml")
    open(scratch, "w").write('msg: "{{ percona. }}"\n')
    try:
        line0 = 'msg: "{{ percona. }}"'
        dot = line0.index("percona.") + len("percona.")
        ok &= check(
            "completion: members of `percona`",
            completion(scratch, 0, dot),
            ["administration", "port"],
        )
        # top-level after `{{ `
        open(scratch, "w").write('msg: "{{  }}"\n')
        top_col = 'msg: "{{ '.index("{{ ") + 3
        ok &= check(
            "completion: top-level vars include percona + vault",
            [v for v in completion(scratch, 0, top_col)
             if v in ("percona", "vault", "cppd_environment")],
            ["cppd_environment", "percona", "vault"],
        )
        # inline-defined vars (set_fact / vars: / register) are completable
        inline = completion(scratch, 0, top_col)
        ok &= check(
            "completion: includes set_fact / vars: / register vars",
            sorted(v for v in inline
                   if v in ("derived_var", "derived_dict", "cmd_result", "play_level_var")),
            ["cmd_result", "derived_dict", "derived_var", "play_level_var"],
        )
    finally:
        os.remove(scratch)

    # --- hover (value + all definition sites) -------------------------------
    def hover_md(path, line, character):
        _next_id[0] += 1
        rid = _next_id[0]
        send(proc, {
            "jsonrpc": "2.0", "id": rid, "method": "textDocument/hover",
            "params": {
                "textDocument": {"uri": uri(path)},
                "position": {"line": line, "character": character},
            },
        })
        msg = read_until_id(proc, rid)
        if "error" in msg:
            print("  hover error:", msg["error"])
            return ""
        res = msg["result"]
        return res["contents"]["value"] if res else ""

    pline = LINES.index('        msg: "{{ percona.administration.password }}"')
    md = hover_md(PLAYBOOK, pline, LINES[pline].rfind("password") + 3)
    ok &= check(
        "hover lists all 5 definition sites",
        all(v in md for v in ["default_password", "group_all_password",
                              "db_group_password", "host_specific_password",
                              "role_vars_password"]),
        True,
    )
    ok &= check(
        "hover shows values and tier labels",
        all(s in md for s in ["host_specific_password", "role vars", "role defaults"]),
        True,
    )

    # hover on a set_fact-defined var used in another play
    extra = os.path.join(FIXTURES, "extra_play.yml")
    elines = open(extra).read().splitlines()
    eline = next(i for i, l in enumerate(elines) if "derived_var" in l)
    emd = hover_md(extra, eline, elines[eline].index("derived_var") + 3)
    ok &= check(
        "hover on set_fact var shows its source + value",
        "set_fact" in emd and "computed" in emd,
        True,
    )

    send(proc, {"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": None})
    read_until_id(proc, 2)
    send(proc, {"jsonrpc": "2.0", "method": "exit", "params": None})
    proc.stdin.close()
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        proc.kill()

    print("\n" + ("ALL PASS" if ok else "SOME FAILED"))
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
