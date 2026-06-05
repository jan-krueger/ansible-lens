# Ansible Lens

A language server for **Ansible & Jinja2 variables** in the
[Zed](https://zed.dev) editor: jump to where a nested variable is defined, find
everywhere it's used, see its values on hover, and complete variable paths as
you type — across `group_vars`, `host_vars`, role `defaults`/`vars`, and
`set_fact`/`register`.

It speaks standard LSP over stdio, so it works with any LSP client; the
packaged extension targets Zed.

## Features

- **Go to Definition** (`F12`) — from any `{{ variable.path }}` or `key:` to
  where it's defined. Understands dotted paths, bracket accessors
  (`data['a']["b"]`), and `hostvars[host]…` prefixes.
- **Find References** (`Shift+F12`) — every place a variable is used, across
  YAML and templates. Works on a whole dict (`console_ui`) or a single leaf.
- **Hover** — shows **every** place a variable is defined, with its value,
  precedence tier, and inventory. Inside a `host_vars/<host>` file it resolves
  the value the way that host actually sees it.
- **Completion** — inside `{{ … }}` / `{% … %}`, completes one variable segment
  at a time from your indexed variables.
- **Diagnostics** — flags `{{ usages }}` of variables defined nowhere
  (typo-catching), tuned for zero false positives on real inventories.
  [Opt-out below](#diagnostics).

A variable is often defined in several files; Ansible Lens shows **all** of its
definitions, ordered by Ansible's precedence (the winner first) rather than
guessing one.

## Install

Once published to the Zed extension registry, install **Ansible Lens** from
**Zed → Extensions**. On first use it downloads the prebuilt server binary for
your platform from
[GitHub Releases](https://github.com/jan-krueger/ansible-lens/releases) — **no
Rust toolchain required**.

To use your own build instead (development, or an unsupported platform), put
`ansible-lens-lsp` on your `PATH` — it always takes precedence over the
downloaded copy:

```sh
cargo install --path server
```

## Templates (`.j2` and other extensions)

Zed attaches a language server by a file's **language**, which it picks from the
file extension. So:

- **`.j2` / `.jinja2`** files work if you have a Jinja language extension
  installed.
- **Templates with ordinary names** (`templates/nginx.conf`, `templates/app.ini`,
  …) need a `file_types` glob in your Zed `settings.json` pointing them at a
  language Ansible Lens attaches to (`YAML`, `Ansible`, or `Jinja2`):

  ```json
  {
    "file_types": {
      "Ansible": ["**/templates/**"]
    }
  }
  ```

  Now everything under `templates/` is treated as `Ansible`, and variable
  navigation works inside those files.

## Diagnostics

Ansible Lens warns when a `{{ usage }}`'s root variable is defined nowhere in
the workspace — catching typos like `consoel_ui.install_dir`. It is
deliberately conservative: it won't flag `set_fact`/loop variables, Jinja
keywords/filters, Ansible magic vars & facts, `SCREAMING_CASE` extra-vars, and
the like.

Disable it through the language server's initialization options in your Zed
`settings.json`:

```json
{
  "lsp": {
    "ansible-lens": {
      "initialization_options": { "diagnostics": false }
    }
  }
}
```

## Contributing

```sh
make help       # list available tasks
make fmt        # format Rust + Python
make lint       # clippy + ruff
make test       # unit tests + end-to-end suites
make git-setup  # enable the pre-commit / pre-push hooks
```

To build the extension locally: `cargo install --path server`, then
**Zed → Extensions → Install Dev Extension** and pick `editors/zed/`.

Design and internals — the precedence model, indexing, performance, and known
limitations — are in [ARCHITECTURE.md](ARCHITECTURE.md).

## License

GNU GPLv3 — see [LICENSE](LICENSE).
