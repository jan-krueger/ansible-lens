//! Jinja2 / Ansible variable reference tokenizer: cursor → what to look up.
//! A single expression has several click targets that resolve differently:
//!
//! ```text
//! data.servers[env].address
//! └────┬─────┘ └─┬─┘ └──┬──┘
//!    prefix    index   suffix
//! ```
//!
//!   - prefix    -> `Query::Exact("data.servers")`     (the dict)
//!   - index var -> `Query::Exact("env")`              (its own var)
//!   - suffix    -> `Query::Glob([data, servers, *, …])` (unknown runtime key)
//!
//! Also handles literal bracket accessors (`a['b']` -> `a.b`) and `hostvars[…]`
//! magic prefixes (stripped to the real path).

use tower_lsp::lsp_types::{Position, Range};

/// One element of a glob lookup pattern. `Wild` matches exactly one dotted
/// level (a runtime index we couldn't resolve statically).
#[derive(Debug, Clone, PartialEq)]
pub enum GlobSeg {
    Lit(String),
    Wild,
}

/// What the cursor resolves to.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    /// A concrete dotted path, e.g. `data.servers`.
    Exact(String),
    /// A path containing runtime indices, e.g. `data.servers.*.address`.
    Glob(Vec<GlobSeg>),
}

/// Magic names that wrap a real variable behind a dynamic index.
const MAGIC_PREFIXES: &[&str] = &["hostvars", "vars", "groupvars"];

// Chain segments reuse `GlobSeg`: `Lit` = a known key, `Wild` = a runtime index.

/// An unquoted index identifier (the `env` in `servers[env]`), itself a
/// resolvable variable.
#[derive(Debug, Clone)]
struct IndexIdent {
    name: String,
    start: usize,
    end: usize,
}

struct Chain {
    /// Inclusive char index of the chain's first character.
    start: usize,
    /// Exclusive char index just past the chain's last character.
    end: usize,
    segs: Vec<GlobSeg>,
    /// Start char index of each segment's accessor (parallel to `segs`).
    seg_starts: Vec<usize>,
    /// Unquoted-identifier indices nested inside this chain.
    index_idents: Vec<IndexIdent>,
}

/// Resolve the query under `character` (a 0-indexed char offset). Returns `None`
/// if the cursor is not on a resolvable reference.
pub fn extract_query(line: &str, character: usize) -> Option<Query> {
    let chars: Vec<char> = line.chars().collect();
    let cursor = character.min(chars.len());

    let chain = scan_chains(&chars)
        .into_iter()
        .find(|c| cursor >= c.start && cursor <= c.end)?;

    // 1. Cursor on an index variable (narrowest target) -> resolve it directly.
    for ii in &chain.index_idents {
        if cursor >= ii.start && cursor <= ii.end {
            return Some(Query::Exact(ii.name.clone()));
        }
    }

    resolve_chain(&chain, cursor)
}

/// Test helper: the dotted string of an exact query (`None` for globs/misses).
#[cfg(test)]
pub fn extract_path_at(line: &str, character: usize) -> Option<String> {
    match extract_query(line, character)? {
        Query::Exact(s) => Some(s),
        Query::Glob(_) => None,
    }
}

/// Index of the first "real" segment after stripping a leading magic var and
/// its dynamic index (`hostvars[host]...` -> the path after `[host]`).
fn magic_strip_index(segs: &[GlobSeg]) -> usize {
    if let Some(GlobSeg::Lit(first)) = segs.first()
        && MAGIC_PREFIXES.contains(&first.to_ascii_lowercase().as_str())
    {
        let mut idx = 1;
        if let Some(GlobSeg::Wild) = segs.get(idx) {
            idx += 1;
        }
        return idx;
    }
    0
}

fn resolve_chain(chain: &Chain, cursor: usize) -> Option<Query> {
    let start_idx = magic_strip_index(&chain.segs);
    let segs = &chain.segs[start_idx..];
    let starts = &chain.seg_starts[start_idx..];
    if segs.is_empty() {
        return None;
    }

    // Position of the first remaining runtime index, if any.
    let first_dynamic = segs
        .iter()
        .position(|s| matches!(s, GlobSeg::Wild))
        .map(|i| starts[i]);

    match first_dynamic {
        // At/before a runtime index, or none at all: the concrete leading path.
        Some(dyn_start) if cursor < dyn_start => leading_literals(segs).map(Query::Exact),
        None => leading_literals(segs).map(Query::Exact),
        // Cursor past a runtime index: glob the whole chain.
        Some(_) => Some(Query::Glob(segs.to_vec())),
    }
}

/// The leading run of literal segments, joined (stops at the first `Wild`).
pub fn leading_literals(pattern: &[GlobSeg]) -> Option<String> {
    let mut parts = Vec::new();
    for seg in pattern {
        match seg {
            GlobSeg::Lit(t) => parts.push(t.clone()),
            GlobSeg::Wild => break,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("."))
    }
}

/// Every variable usage on a line (only inside `{{ … }}` / `{% … %}`), as
/// `(pattern, range)`. Each chain yields its path plus any nested index var.
pub fn usages_in_line(line: &str, line_no: u32) -> Vec<(Vec<GlobSeg>, Range)> {
    let chars: Vec<char> = line.chars().collect();
    let spans = jinja_spans(&chars);
    if spans.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for chain in scan_chains(&chars) {
        let inside = spans
            .iter()
            .any(|&(s, e)| chain.start >= s && chain.start < e);
        if !inside {
            continue;
        }
        if let Some(pattern) = chain_to_pattern(&chain) {
            out.push((pattern, span_range(line_no, chain.start, chain.end)));
        }
        for ii in &chain.index_idents {
            out.push((
                vec![GlobSeg::Lit(ii.name.clone())],
                span_range(line_no, ii.start, ii.end),
            ));
        }
    }
    out
}

/// `(root, range)` for each chain that's a genuine variable reference, for
/// undefined-var checking. Excludes filters (`| name`), calls (`name(`) and
/// quoted-string contents; keyword/magic-var filtering is the caller's job.
pub fn undefined_candidates(line: &str, line_no: u32) -> Vec<(String, Range)> {
    let chars: Vec<char> = line.chars().collect();
    let spans = jinja_spans(&chars);
    if spans.is_empty() {
        return Vec::new();
    }

    // String literals *inside* the Jinja expression (e.g. `default('x')`) — the
    // identifiers within them are not variables.
    let strings = quoted_regions(&chars, &spans);

    let mut out = Vec::new();
    for chain in scan_chains(&chars) {
        let inside = spans
            .iter()
            .any(|&(s, e)| chain.start >= s && chain.start < e);
        if !inside {
            continue;
        }
        if strings
            .iter()
            .any(|&(s, e)| chain.start >= s && chain.start < e)
        {
            continue; // inside a quoted string literal
        }
        let Some(GlobSeg::Lit(root)) = chain.segs.first() else {
            continue;
        };
        if preceded_by(&chars, chain.start, '|') || followed_by(&chars, chain.end, '(') {
            continue; // a filter or a function/method call, not a variable
        }
        out.push((root.clone(), span_range(line_no, chain.start, chain.end)));
    }
    out
}

/// Variables introduced by Jinja statements within `content`: `{% for x in … %}`
/// loop targets and `{% set x = … %}` assignments. These are file-local and
/// must not be reported as undefined.
pub fn jinja_scope_vars(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(open) = rest.find("{%") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("%}") else {
            break;
        };
        let stmt = after[..close].trim().trim_start_matches('-').trim();
        if let Some(targets) = stmt.strip_prefix("for ") {
            push_idents(targets.split(" in ").next().unwrap_or(""), &mut out);
        } else if let Some(lhs) = stmt.strip_prefix("set ") {
            push_idents(lhs.split('=').next().unwrap_or(""), &mut out);
        }
        rest = &after[close + 2..];
    }
    out
}

/// Push identifier-looking tokens from `text` (splitting on non-ident chars).
fn push_idents(text: &str, out: &mut Vec<String>) {
    for tok in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if tok.chars().next().is_some_and(is_ident_start) {
            out.push(tok.to_string());
        }
    }
}

/// Char ranges of quoted string literals occurring *inside* the given Jinja
/// spans (outer YAML quotes are excluded because they fall outside the spans).
fn quoted_regions(chars: &[char], spans: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut regions = Vec::new();
    for &(s, e) in spans {
        let mut i = s;
        while i < e {
            if chars[i] == '\'' || chars[i] == '"' {
                let quote = chars[i];
                let start = i;
                i += 1;
                while i < e && chars[i] != quote {
                    i += 1;
                }
                let end = (i + 1).min(e); // include the closing quote
                regions.push((start, end));
                i = end;
            } else {
                i += 1;
            }
        }
    }
    regions
}

fn preceded_by(chars: &[char], start: usize, c: char) -> bool {
    let mut i = start;
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i > 0 && chars[i - 1] == c
}

fn followed_by(chars: &[char], end: usize, c: char) -> bool {
    let mut i = end;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    chars.get(i) == Some(&c)
}

/// Is the cursor inside a `{{ … }}` / `{% … %}` expression? (Gates completion.)
pub fn cursor_in_expr(line: &str, char_col: usize) -> bool {
    let chars: Vec<char> = line.chars().collect();
    jinja_spans(&chars)
        .iter()
        .any(|&(s, e)| char_col >= s && char_col <= e)
}

/// A whole chain as a lookup pattern (magic-stripped). `None` if nothing real
/// remains or it doesn't start with a literal variable root.
fn chain_to_pattern(chain: &Chain) -> Option<Vec<GlobSeg>> {
    let segs = &chain.segs[magic_strip_index(&chain.segs)..];
    if !matches!(segs.first(), Some(GlobSeg::Lit(_))) {
        return None;
    }
    Some(segs.to_vec())
}

/// Char spans *inside* each `{{ … }}` / `{% … %}` region on a line.
/// Single-line only (multi-line Jinja blocks are not stitched together).
fn jinja_spans(chars: &[char]) -> Vec<(usize, usize)> {
    let n = chars.len();
    let mut spans = Vec::new();
    let mut i = 0;
    while i + 1 < n {
        if chars[i] == '{' && (chars[i + 1] == '{' || chars[i + 1] == '%') {
            // `{{` closes with `}}`, `{%` closes with `%}`.
            let first_close = if chars[i + 1] == '{' { '}' } else { '%' };
            let mut j = i + 2;
            let mut end = n;
            while j + 1 < n {
                if chars[j] == first_close && chars[j + 1] == '}' {
                    end = j;
                    break;
                }
                j += 1;
            }
            spans.push((i + 2, end));
            i = if end < n { end + 2 } else { n };
        } else {
            i += 1;
        }
    }
    spans
}

fn span_range(line_no: u32, start: usize, end: usize) -> Range {
    Range {
        start: Position::new(line_no, start as u32),
        end: Position::new(line_no, end as u32),
    }
}

/// Scan a line into accessor chains (non-overlapping), recording segment
/// positions and any nested index-variable identifiers.
fn scan_chains(chars: &[char]) -> Vec<Chain> {
    let mut chains = Vec::new();
    let n = chars.len();
    let mut i = 0;

    while i < n {
        if !is_ident_start(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        let mut segs = Vec::new();
        let mut seg_starts = Vec::new();
        let mut index_idents = Vec::new();

        // Leading identifier.
        let (ident, next) = take_ident(chars, i);
        seg_starts.push(i);
        segs.push(GlobSeg::Lit(ident));
        i = next;

        // Trailing accessors.
        loop {
            if i < n && chars[i] == '.' && i + 1 < n && is_ident_start(chars[i + 1]) {
                let field_start = i + 1;
                let (ident, next) = take_ident(chars, field_start);
                seg_starts.push(field_start);
                segs.push(GlobSeg::Lit(ident));
                i = next;
            } else if i < n && chars[i] == '[' {
                match parse_index(chars, i) {
                    Some((seg, end, ident)) => {
                        seg_starts.push(i);
                        segs.push(seg);
                        if let Some(ii) = ident {
                            index_idents.push(ii);
                        }
                        i = end;
                    }
                    None => break, // unterminated '['
                }
            } else {
                break;
            }
        }

        chains.push(Chain {
            start,
            end: i,
            segs,
            seg_starts,
            index_idents,
        });
    }

    chains
}

/// Parse a `[...]` accessor at the `[` index. Returns the segment, the index
/// past `]`, and the inner identifier if the index is a bare variable name.
fn parse_index(chars: &[char], open: usize) -> Option<(GlobSeg, usize, Option<IndexIdent>)> {
    debug_assert_eq!(chars[open], '[');
    let close = (open + 1..chars.len()).find(|&j| chars[j] == ']')?;
    let inner: String = chars[open + 1..close].iter().collect();
    let trimmed = inner.trim();

    // Quoted literal -> a statically known key.
    if let Some(lit) = unquote(trimmed) {
        return Some((GlobSeg::Lit(lit), close + 1, None));
    }

    // A bare identifier -> a runtime index that is itself a variable.
    let mut j = open + 1;
    while j < close && chars[j].is_whitespace() {
        j += 1;
    }
    if j < close && is_ident_start(chars[j]) {
        let (name, jend) = take_ident(chars, j);
        let mut k = jend;
        while k < close && chars[k].is_whitespace() {
            k += 1;
        }
        if k == close {
            let ident = IndexIdent {
                name,
                start: j,
                end: jend,
            };
            return Some((GlobSeg::Wild, close + 1, Some(ident)));
        }
    }

    // Some other expression (`[a + 1]`, `[0]`, ...) — unresolvable.
    Some((GlobSeg::Wild, close + 1, None))
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn take_ident(chars: &[char], mut i: usize) -> (String, usize) {
    let start = i;
    while i < chars.len() && is_ident_continue(chars[i]) {
        i += 1;
    }
    (chars[start..i].iter().collect(), i)
}

/// If `s` is a single- or double-quoted string literal, return its contents.
fn unquote(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'\'' || bytes[0] == b'"')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        Some(s[1..s.len() - 1].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: &str, col: usize) -> Option<String> {
        extract_path_at(line, col)
    }

    #[test]
    fn plain_dotted_path_any_cursor_position() {
        let line = "    password: \"{{ percona.administration.password }}\"";
        for needle in ["percona", "administration"] {
            let col = line.find(needle).unwrap() + 2;
            assert_eq!(
                at(line, col),
                Some("percona.administration.password".to_string())
            );
        }
    }

    #[test]
    fn bracket_literals_normalise_to_dots() {
        let line = "{{ data['some'][\"nested\"].path }}";
        let col = line.find("some").unwrap();
        assert_eq!(at(line, col), Some("data.some.nested.path".to_string()));
    }

    #[test]
    fn hostvars_magic_prefix_is_stripped() {
        let line = "{{ hostvars[inventory_hostname]['some']['nested']['path'] }}";
        let col = line.find("nested").unwrap();
        assert_eq!(at(line, col), Some("some.nested.path".to_string()));
    }

    #[test]
    fn index_variable_resolves_to_itself() {
        let line = "console: \"{{ vault.cppd[cppd_environment].console_api_key }}\"";
        let col = line.find("cppd_environment").unwrap() + 2;
        assert_eq!(
            extract_query(line, col),
            Some(Query::Exact("cppd_environment".to_string()))
        );
    }

    #[test]
    fn prefix_before_dynamic_index_is_exact() {
        let line = "console: \"{{ vault.cppd[cppd_environment].console_api_key }}\"";
        let col = line.find("cppd[").unwrap(); // on `cppd`
        assert_eq!(
            extract_query(line, col),
            Some(Query::Exact("vault.cppd".to_string()))
        );
    }

    #[test]
    fn suffix_after_dynamic_index_is_glob() {
        let line = "console: \"{{ vault.cppd[cppd_environment].console_api_key }}\"";
        let col = line.find("console_api_key").unwrap() + 2;
        assert_eq!(
            extract_query(line, col),
            Some(Query::Glob(vec![
                GlobSeg::Lit("vault".into()),
                GlobSeg::Lit("cppd".into()),
                GlobSeg::Wild,
                GlobSeg::Lit("console_api_key".into()),
            ]))
        );
    }

    #[test]
    fn numeric_index_is_dynamic_not_a_var() {
        let line = "{{ users[0].name }}";
        // clicking the suffix globs over the numeric index
        let col = line.find("name").unwrap();
        assert_eq!(
            extract_query(line, col),
            Some(Query::Glob(vec![
                GlobSeg::Lit("users".into()),
                GlobSeg::Wild,
                GlobSeg::Lit("name".into()),
            ]))
        );
    }

    #[test]
    fn cursor_off_any_token_returns_none() {
        assert_eq!(extract_query("   # a comment", 1), None);
    }

    #[test]
    fn undefined_candidates_excludes_filters_and_calls() {
        let roots = |line: &str| -> Vec<String> {
            undefined_candidates(line, 0)
                .into_iter()
                .map(|(r, _)| r)
                .collect()
        };
        // a plain variable is a candidate
        assert_eq!(roots("x: \"{{ my_var }}\""), vec!["my_var"]);
        // filter name after `|` is excluded; the variable before it is kept
        assert_eq!(roots("x: \"{{ my_var | default('z') }}\""), vec!["my_var"]);
        // function/method calls are excluded
        assert_eq!(
            roots("x: \"{{ lookup('env', 'X') }}\""),
            Vec::<String>::new()
        );
        assert_eq!(roots("x: \"{{ foo.bar() }}\""), Vec::<String>::new());
        // outside Jinja delimiters: nothing
        assert_eq!(roots("just: plain.text.here"), Vec::<String>::new());
    }
}
