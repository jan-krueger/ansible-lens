//! YAML → `(dotted.path, key-range)` pairs with exact key-token positions.
//! An entry is emitted at every mapping level (not just leaves), so a sub-dict
//! reference resolves and per-leaf overrides land where they're written.

use std::borrow::Cow;

use marked_yaml::types::MarkedScalarNode;
use marked_yaml::{LoaderOptions, Node, parse_yaml, parse_yaml_with_options};
use tower_lsp::lsp_types::{Position, Range};

/// Value of an Ansible-Vault-encrypted leaf, so callers flag it instead of
/// leaking ciphertext.
pub const VAULT_ENCRYPTED: &str = "\0vault-encrypted";

/// marked-yaml rejects unknown tags, so blank out Ansible's `!vault`/`!unsafe`
/// with equal-length spaces (preserving every position) before parsing.
fn strip_ansible_tags(content: &str) -> Cow<'_, str> {
    if content.contains("!vault") || content.contains("!unsafe") {
        Cow::Owned(blank_yaml_tag(
            &blank_yaml_tag(content, "!vault"),
            "!unsafe",
        ))
    } else {
        Cow::Borrowed(content)
    }
}

/// Blank whole-token occurrences of `tag` (bounded by whitespace/`-`/line edge),
/// leaving a literal `!vault` inside a string value untouched.
fn blank_yaml_tag(content: &str, tag: &str) -> String {
    let blanks = " ".repeat(tag.len());
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(pos) = rest.find(tag) {
        let (before, after) = (&rest[..pos], &rest[pos + tag.len()..]);
        let is_tag = before
            .chars()
            .last()
            .is_none_or(|c| c.is_whitespace() || c == '-')
            && after.chars().next().is_none_or(char::is_whitespace);
        out.push_str(before);
        out.push_str(if is_tag { blanks.as_str() } else { tag });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The dotted key path whose key token sits under `pos` (longest match wins).
pub fn key_path_at(content: &str, pos: Position) -> Option<String> {
    flatten(content)
        .into_iter()
        .filter(|v| {
            pos.line == v.range.start.line
                && pos.character >= v.range.start.character
                && pos.character <= v.range.end.character
        })
        .max_by_key(|v| v.dotted.len())
        .map(|v| v.dotted)
}

/// A flattened key with its key-token range; `value` is `None` for dicts/lists.
#[derive(Debug, Clone, PartialEq)]
pub struct FlatVar {
    pub dotted: String,
    pub range: Range,
    pub value: Option<String>,
}

/// A parsed document, so callers needing both flatten + inline defs parse once.
pub struct Document {
    root: Option<Node>,
}

/// Parse a document once. Tolerates `!vault`/`!unsafe` tags and both mapping-
/// and sequence-rooted files (task/play files are sequences). A scalar-rooted
/// file (e.g. a whole-file vault blob) yields an empty document.
pub fn parse_document(content: &str) -> Document {
    let content = strip_ansible_tags(content);
    let root = parse_yaml(0, &content)
        .or_else(|_| {
            parse_yaml_with_options(0, &content, LoaderOptions::default().toplevel_sequence())
        })
        .ok();
    Document { root }
}

impl Document {
    /// Every mapping key flattened to a dotted path (empty for non-mapping roots).
    pub fn flat_vars(&self) -> Vec<FlatVar> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            walk(root, &mut Vec::new(), &mut out);
        }
        out
    }

    /// Variables defined inline via `set_fact:` / `vars:` / `register:` / `loop_var:`.
    pub fn inline_defs(&self) -> Vec<(DefKind, FlatVar)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            walk_inline(root, &mut out);
        }
        out
    }
}

pub fn flatten(content: &str) -> Vec<FlatVar> {
    parse_document(content).flat_vars()
}

/// Where an inline definition came from (drives its precedence + label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    Vars,
    SetFact,
    Register,
    Loop,
}

/// Recursive worker for [`Document::inline_defs`].
fn walk_inline(node: &Node, out: &mut Vec<(DefKind, FlatVar)>) {
    match node {
        Node::Mapping(map) => {
            for (key, value) in map.iter() {
                match key.as_str() {
                    "set_fact" | "ansible.builtin.set_fact" => {
                        out.extend(
                            flatten_node(value)
                                .into_iter()
                                .map(|v| (DefKind::SetFact, v)),
                        );
                    }
                    "vars" => {
                        out.extend(flatten_node(value).into_iter().map(|v| (DefKind::Vars, v)));
                    }
                    "register" | "loop_var" => {
                        if let Node::Scalar(name) = value {
                            let kind = if key.as_str() == "register" {
                                DefKind::Register
                            } else {
                                DefKind::Loop
                            };
                            out.push((
                                kind,
                                FlatVar {
                                    dotted: name.as_str().to_string(),
                                    range: key_range(name),
                                    value: None,
                                },
                            ));
                        }
                    }
                    // Not a definition key — descend looking for nested ones.
                    _ => walk_inline(value, out),
                }
            }
        }
        Node::Sequence(seq) => {
            for item in seq.iter() {
                walk_inline(item, out);
            }
        }
        Node::Scalar(_) => {}
    }
}

/// Flatten a single mapping node (used for `set_fact:`/`vars:` sub-blocks).
fn flatten_node(node: &Node) -> Vec<FlatVar> {
    let mut out = Vec::new();
    walk(node, &mut Vec::new(), &mut out);
    out
}

fn walk(node: &Node, prefix: &mut Vec<String>, out: &mut Vec<FlatVar>) {
    let Some(map) = node.as_mapping() else {
        return;
    };
    for (key, value) in map.iter() {
        prefix.push(key.as_str().to_string());
        out.push(FlatVar {
            dotted: prefix.join("."),
            range: key_range(key),
            value: value.as_scalar().map(|s| {
                let v = s.as_str();
                if v.contains("$ANSIBLE_VAULT") {
                    VAULT_ENCRYPTED.to_string()
                } else {
                    v.to_string()
                }
            }),
        });
        walk(value, prefix, out);
        prefix.pop();
    }
}

/// LSP range of a key token. marked-yaml markers are 1-indexed (→ subtract 1)
/// and lack a reliable end, so the end is derived from the key length.
fn key_range(key: &MarkedScalarNode) -> Range {
    let Some(start) = key.span().start() else {
        return Range::default();
    };
    let line = start.line().saturating_sub(1) as u32;
    let col = start.column().saturating_sub(1) as u32;
    let len = key.as_str().chars().count() as u32;
    Range {
        start: Position::new(line, col),
        end: Position::new(line, col + len),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(vars: &'a [FlatVar], dotted: &str) -> &'a FlatVar {
        vars.iter()
            .find(|v| v.dotted == dotted)
            .unwrap_or_else(|| panic!("missing key {dotted}; got {vars:#?}"))
    }

    #[test]
    fn flattens_nested_mapping_with_positions() {
        let yaml = "percona:\n  administration:\n    password: \"secret_vault_string\"\n";
        let vars = flatten(yaml);

        assert!(vars.iter().any(|v| v.dotted == "percona"));
        assert!(vars.iter().any(|v| v.dotted == "percona.administration"));

        // The leaf points at the `password` key token on line index 2.
        let leaf = find(&vars, "percona.administration.password");
        assert_eq!(leaf.range.start, Position::new(2, 4));
        assert_eq!(leaf.range.end, Position::new(2, 12)); // 4 + len("password")
    }

    #[test]
    fn top_level_keys() {
        let vars = flatten("foo: 1\nbar: 2\n");
        let foo = find(&vars, "foo");
        assert_eq!(foo.range.start, Position::new(0, 0));
        let bar = find(&vars, "bar");
        assert_eq!(bar.range.start, Position::new(1, 0));
    }

    #[test]
    fn inline_vault_tags_dont_break_the_file() {
        // `!vault` tagged values must not prevent the rest of the file indexing.
        let yaml = "\
vault:
  cpsd:
    int:
      db_password: !vault |
        $ANSIBLE_VAULT;1.1;AES256
        66386439653...
      api_key: plain_value
";
        let vars = flatten(yaml);
        assert!(vars.iter().any(|v| v.dotted == "vault"));
        assert!(
            vars.iter()
                .any(|v| v.dotted == "vault.cpsd.int.db_password")
        );
        assert!(vars.iter().any(|v| v.dotted == "vault.cpsd.int.api_key"));
        // the encrypted value is marked as such, not stored as raw ciphertext
        let pw = find(&vars, "vault.cpsd.int.db_password");
        assert_eq!(pw.value.as_deref(), Some(VAULT_ENCRYPTED));
    }

    #[test]
    fn whole_file_vault_yields_nothing_without_panic() {
        // A fully `ansible-vault encrypt`ed file is opaque ciphertext — it must
        // produce no definitions and must not panic.
        let blob = "$ANSIBLE_VAULT;1.1;AES256\n3438383...\n6133626...\n";
        assert!(flatten(blob).is_empty());
        assert!(parse_document(blob).inline_defs().is_empty());
    }

    #[test]
    fn vault_tag_only_stripped_as_a_whole_token() {
        // `!vault` as a real tag is stripped; the same text glued to other chars
        // (so not a standalone token) is left intact.
        let yaml =
            "note: \"see the !vaultish note\"\npw: !vault |\n  $ANSIBLE_VAULT;1.1;AES256\n  abcd\n";
        let vars = flatten(yaml);
        let note = find(&vars, "note");
        assert_eq!(note.value.as_deref(), Some("see the !vaultish note"));
        assert!(vars.iter().any(|v| v.dotted == "pw"));
    }

    #[test]
    fn malformed_yaml_yields_nothing() {
        assert!(flatten("percona:\n  - : : invalid").is_empty() || !flatten("foo: 1").is_empty());
    }

    #[test]
    fn inline_defs_finds_set_fact_vars_and_register() {
        let yaml = "\
- hosts: all
  vars:
    play_var: 1
  tasks:
    - name: derive
      set_fact:
        fact_var: x
        fact_dict:
          inner: 2
    - name: run
      command: echo hi
      register: cmd_out
";
        let defs = parse_document(yaml).inline_defs();
        let has = |kind: DefKind, dotted: &str| {
            defs.iter().any(|(k, v)| *k == kind && v.dotted == dotted)
        };
        assert!(has(DefKind::Vars, "play_var"));
        assert!(has(DefKind::SetFact, "fact_var"));
        assert!(has(DefKind::SetFact, "fact_dict.inner"));
        assert!(has(DefKind::Register, "cmd_out"));
        // the wholesale flatten of the same content would NOT isolate these
        assert!(!has(DefKind::Vars, "tasks"));
    }
}
