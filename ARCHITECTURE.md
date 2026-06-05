# Ansible Lens — Architecture

Design notes and internals. For installation and usage, see the
[README](README.md).

## A separate native binary

Zed extensions run in a `wasm32-wasip1` sandbox and can't do arbitrary disk
I/O. So the Zed extension (`editors/zed/`) is just a *launcher*: it finds (or
downloads) and starts the native server (`server/`), which does all the file
scanning and indexing and talks to Zed over stdio.

## Variables defined in more than one place

The same Ansible variable is legitimately defined in many files, and Ansible
resolves conflicts by a fixed **precedence order**. This server models that:
each variable key maps to a *list* of definitions, and Go-to-Definition returns
**all** of them, ordered with the effective (winning) one first. Zed shows a
picker; the top entry is the one Ansible would actually use.

Precedence, lowest → highest (see `server/src/precedence.rs`):

| Tier | Source |
|------|--------|
| lowest  | `roles/*/defaults/main.yml` |
|         | `group_vars/all` |
|         | `group_vars/<group>` |
|         | `host_vars/<host>` |
|         | `vars:` blocks (play / task / block) |
|         | `roles/*/vars/main.yml` |
| highest | `set_fact:` / `register:` |

Because every nesting level is indexed per-leaf, dict *merging* works too: if
`percona.administration.password` is set in one file and
`percona.administration.username` in another, each leaf resolves to where it is
actually written.

## How it works

**Phase A — indexing** (`server/src/index.rs`, `flatten.rs`, `precedence.rs`):
on `initialize`, the workspace is scanned in the background. Each Ansible var
file is parsed with [`marked-yaml`](https://docs.rs/marked-yaml) (which retains
source line/column), flattened into dot-notation keys with exact key-token
ranges, and tagged with its precedence tier. Variables defined *inline* in
task/play files are indexed too — `set_fact:` and `vars:` mappings (flattened)
and `register:` names — so a `{{ var }}` set by a `set_fact` resolves to that
task, not nowhere.

**Phase B — definition** (`server/src/jinja.rs`, `backend.rs`): on a
`textDocument/definition` request, the line under the cursor is tokenized to
extract the full dotted path — handling Jinja braces, literal bracket accessors
(`data['a']["b"]` → `data.a.b`), and `hostvars[host][...]` prefixes — then looked
up in the index and returned as a precedence-ordered `Location[]`.

### Runtime indices

A single expression has several click targets, and they resolve differently.
For `vault.cppd[cppd_environment].console_api_key`:

| Click | Resolves to |
|-------|-------------|
| `vault` / `cppd` | the `vault.cppd` dict (exact) |
| `cppd_environment` | wherever **that** variable is defined (it's its own reference) |
| `console_api_key` | a glob `vault.cppd.*.console_api_key` — **every** concrete environment, since the index can't be evaluated statically |

The glob is a best-effort fallback used only when an expression crosses a
runtime index; it may over-match, so it returns all candidates rather than
guessing one.

## Find references

`textDocument/references` is the mirror of definition: from a variable — either
its `key:` in a vars file or a `{{ usage }}` — find every place it is
referenced. A second **usage index** is built by scanning all YAML plus
everything under `templates/` (any extension) for Jinja expressions.

Matching uses a **prefix-glob** rule — a usage references the target when it is
at least as deep and its leading segments match:

| Cursor on | Finds |
|-----------|-------|
| leaf `console_ui.install_dir` | exact usages |
| parent `console_ui` | every `console_ui.*` usage |
| `vault.cppd.prod.console_api_key` | the dynamic-index usage `{{ vault.cppd[env].console_api_key }}` (reverse glob) |

Because Ansible variables are dynamically scoped, this is **name-based**: it
answers "where is this variable used?", listing every textual reference, not
"which usages bind to *this* override at runtime".

## Hover

Hovering a variable shows **every** place it's defined — with its value,
precedence tier, and **inventory** — rather than asserting a single winner:

```
cpsd.mysql.address

Defined in 5 place(s):
- `89.191.81.37` — host_vars · inv_prod · globalcps-nue-n1-prod…
- `10.111.4.37`  — host_vars · inv_prod · globalcps-fra-n1-prod…
- `10.70.0.20`   — group_vars · inv_test · cpsd-node
- `10.70.0.20`   — group_vars · inv_int · cpsd-node
- `~`            — role defaults
```

Each site is labeled **tier · inventory · host/group**, so even two different
hosts' `host_vars` under the same inventory are distinguishable.

This is deliberately **not** a single resolved value. Separate inventories
(`inv_prod`, `inv_test`, …) are parallel, mutually-exclusive contexts, not an
override stack, and with `hash_behaviour = merge` values combine rather than
replace — so the honest answer is "here is every site, labeled," ordered by
precedence only as a hint. Definitions are tagged with the inventory directory
(the parent of their `group_vars/`/`host_vars/`); role defaults/vars are
inventory-independent.

When hovering inside a `host_vars/<host>` file, the server can instead resolve
the value the way that specific host sees it, showing only the applicable sites
in precedence order.

## Completion

`textDocument/completion`, gated to inside `{{ … }}` / `{% … %}` so it never
fires in plain template text. It completes one path segment at a time, sourced
from the definition index:

- `{{ ` → top-level variables
- `{{ con` → top-level vars starting with `con`
- `{{ console_ui.` → that dict's members (`install_dir`, `bind_host`, …)

Leaf values show as variables; dicts with children show as fields, and each
item's documentation lists the precedence tiers where it's defined. Trigger
characters are `{` and `.`.

## Diagnostics (undefined variables)

Flags `{{ usages }}` whose **root** segment is defined nowhere — catching typos
like `consoel_ui.install_dir`. It is deliberately conservative to avoid crying
wolf in a dynamic system; a usage is only flagged after excluding:

- defined variables (any tier, including `set_fact`/`vars:`/`register`/`loop_var`);
- variables defined **in the same file** (even before the debounced reindex);
- Jinja **filters** (`| default`), **function/method calls** (`lookup(...)`,
  `x.y()`), and identifiers inside **quoted strings**;
- Jinja **keywords / block tags** (`if`, `endfor`, `set`, …) and `{% for %}` /
  `{% set %}` **scope variables**;
- Ansible **magic vars & facts** (`ansible_*`, `inventory_hostname`, `hostvars`,
  `item`, …), `getent_*` module outputs;
- **`SCREAMING_CASE`** names (by convention extra-vars passed with `-e`).

Validated to **zero false positives** across a real multi-role inventory. Only
the *root* is checked (not deep sub-keys), since dicts are often built
dynamically. Disable with `initializationOptions: { "diagnostics": false }`.

## Performance

Designed to stay snappy under rapid navigation on large inventories:

- **Definitions live in a segment trie** (`index.rs`), so exact lookup, glob
  lookup, and completion are all tree descents — `O(path depth + results)`, with
  allocation only for the results. No keys are split or scanned per request.
- **Usages are bucketed by their root segment**, so "find references" only scans
  references that share the target's first segment, not every usage everywhere.
- **Reindexing on edit is debounced** (~200 ms after typing stops) and runs in
  time proportional to the *changed file*, not the whole workspace — per-file
  source maps drive incremental add/remove. Live queries read the open buffer
  directly, so navigation stays current instantly; only cross-file index
  freshness waits for the debounce.
- **On-disk changes are watched** via `workspace/didChangeWatchedFiles`: the
  server registers watchers for `**/*.yml`, `**/*.yaml`, and `**/templates/**`
  on startup, so branch switches, pulls, and edits by other tools keep the
  index correct without a restart. Each event is an incremental per-file update
  (open buffers are skipped — their live text wins).

## Layout

```
server/             native LSP binary (tower-lsp)
  src/main.rs       stdio LSP loop
  src/backend.rs    lifecycle + definition + references + hover + completion; debounced reindex
  src/index.rs      definition trie + root-bucketed usage index; per-file incremental
  src/flatten.rs    YAML → dotted keys with source ranges; key-at-cursor
  src/jinja.rs      cursor → dotted path; usage extraction; in-expr guard
  src/precedence.rs file → Ansible variable source + precedence
  src/config.rs     ansible.cfg hash_behaviour + parsed inventory group graphs
editors/zed/        Zed WASM launcher extension
tests/fixtures/     sample inventory exercising all five precedence tiers
tests/*.py          end-to-end LSP drivers
```

## Known limitations / next steps

- A burst of on-disk changes (e.g. a branch switch touching hundreds of files)
  fires one reindex per file; each is cheap, but the events aren't yet coalesced
  on a timer the way keystrokes are.
- Usage scanning is line-based: a Jinja expression split across multiple lines
  is not stitched together (rare in practice).
- Ansible-Vault inline tags (`!vault` / `!unsafe`) are tolerated — the keys are
  indexed normally and encrypted leaf values display as `🔒 SECRET (vault)` —
  but fully-encrypted files (whole-file `ansible-vault encrypt`) can't be parsed.
- Inline-defined vars (`set_fact`/`vars:`/`register`) resolve when *used*, but
  standing *on* the definition key itself doesn't yet trigger hover/references
  (the cursor-on-key path only understands whole-file vars). `vars_files:`
  targets are not yet resolved.
- Runtime indices are handled heuristically: the index variable resolves to
  itself, and a suffix past the index globs over all candidates. A glob can
  over-match if the same leaf name appears under unrelated parents.
