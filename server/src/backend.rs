//! The tower-lsp `LanguageServer` impl: lifecycle, document sync, and the
//! definition / references / hover / completion / diagnostics handlers.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::config::AnsibleConfig;
use crate::flatten::{key_path_at, parse_document, VAULT_ENCRYPTED};
use crate::index::{Completion, Definition, VarIndex};
use crate::jinja::{
    cursor_in_expr, extract_query, jinja_scope_vars, leading_literals, undefined_candidates, Query,
};
use crate::precedence::{path_components, strip_yaml_ext, VarSource};

pub struct Backend {
    client: Client,
    roots: RwLock<Vec<PathBuf>>,
    index: Arc<RwLock<VarIndex>>,
    config: RwLock<AnsibleConfig>,
    docs: RwLock<HashMap<Url, String>>,
    /// Per-URI edit counter; a debounced reindex only runs if still the latest.
    versions: Arc<RwLock<HashMap<Url, u64>>>,
    /// Client supports dynamic file-watcher registration.
    watch_files: AtomicBool,
    /// Undefined-variable diagnostics enabled (opt-out).
    diagnostics: AtomicBool,
}

/// Glob patterns for on-disk files we watch — the same set we index.
const WATCH_GLOBS: &[&str] = &["**/*.yml", "**/*.yaml", "**/templates/**", "**/ansible.cfg"];

/// How long to wait after the last keystroke before reindexing an edited file.
const REINDEX_DEBOUNCE: Duration = Duration::from_millis(200);

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            roots: RwLock::new(Vec::new()),
            index: Arc::new(RwLock::new(VarIndex::default())),
            config: RwLock::new(AnsibleConfig::default()),
            docs: RwLock::new(HashMap::new()),
            versions: Arc::new(RwLock::new(HashMap::new())),
            watch_files: AtomicBool::new(false),
            diagnostics: AtomicBool::new(true),
        }
    }

    async fn publish_diagnostics(&self, uri: Url, text: &str) {
        if !self.diagnostics.load(Ordering::Relaxed) {
            return;
        }
        let diags = {
            let index = self.index.read().await;
            compute_diagnostics(text, &index)
        };
        self.client.publish_diagnostics(uri, diags, None).await;
    }

    /// Register on-disk file watchers (branch switches, pulls, external edits).
    /// No-op if the client doesn't support dynamic registration.
    async fn register_file_watchers(&self) {
        if !self.watch_files.load(Ordering::Relaxed) {
            return;
        }
        let watchers = WATCH_GLOBS
            .iter()
            .map(|glob| FileSystemWatcher {
                glob_pattern: GlobPattern::String((*glob).into()),
                kind: None, // create | change | delete
            })
            .collect();
        let registration = Registration {
            id: "ansible-lens-watch".into(),
            method: "workspace/didChangeWatchedFiles".into(),
            register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                watchers,
            })
            .ok(),
        };
        if let Err(err) = self.client.register_capability(vec![registration]).await {
            self.client
                .log_message(MessageType::WARNING, format!("file watch unavailable: {err}"))
                .await;
        }
    }

    /// Best-effort fetch of a document's text: prefer the open buffer, fall back
    /// to reading from disk.
    async fn text_for(&self, uri: &Url) -> Option<String> {
        if let Some(text) = self.docs.read().await.get(uri).cloned() {
            return Some(text);
        }
        let path = uri.to_file_path().ok()?;
        std::fs::read_to_string(path).ok()
    }

    async fn bump_version(&self, uri: &Url) -> u64 {
        let mut versions = self.versions.write().await;
        let v = versions.entry(uri.clone()).or_insert(0);
        *v += 1;
        *v
    }

    /// Invalidate any pending debounced reindex for `uri` (a discrete open/save
    /// is about to reindex with authoritative text).
    async fn invalidate_pending(&self, uri: &Url) {
        self.bump_version(uri).await;
    }

    async fn reindex(&self, uri: &Url, content: &str) {
        if let Ok(path) = uri.to_file_path() {
            self.index.write().await.reindex(&path, content);
        }
    }

    /// Debounced reindex: reindex after a quiet period, only if no newer edit
    /// superseded this one — keeps per-keystroke cost off the index write lock.
    async fn schedule_reindex(&self, uri: Url, content: String) {
        let version = self.bump_version(&uri).await;
        let versions = self.versions.clone();
        let index = self.index.clone();
        let client = self.client.clone();
        let diagnostics_on = self.diagnostics.load(Ordering::Relaxed);
        tokio::spawn(async move {
            tokio::time::sleep(REINDEX_DEBOUNCE).await;
            if versions.read().await.get(&uri) != Some(&version) {
                return; // a newer edit arrived; it owns the reindex
            }
            if let Ok(path) = uri.to_file_path() {
                index.write().await.reindex(&path, &content);
            }
            if diagnostics_on {
                let diags = compute_diagnostics(&content, &*index.read().await);
                client.publish_diagnostics(uri, diags, None).await;
            }
        });
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        *self.roots.write().await = collect_roots(&params);
        self.watch_files.store(supports_file_watching(&params), Ordering::Relaxed);
        // Opt out with initializationOptions: { "diagnostics": false }.
        let diagnostics_on = params
            .initialization_options
            .as_ref()
            .and_then(|o| o.get("diagnostics"))
            .and_then(|d| d.as_bool())
            .unwrap_or(true);
        self.diagnostics.store(diagnostics_on, Ordering::Relaxed);

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "ansible-lens-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["{".into(), ".".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let roots = self.roots.read().await.clone();
        // walkdir + fs reads are blocking; keep them off the async runtime.
        let built = tokio::task::spawn_blocking(move || {
            let index = VarIndex::build(&roots);
            let config = AnsibleConfig::load(&roots);
            (index, config)
        })
        .await;

        match built {
            Ok((new_index, config)) => {
                let (files, keys, usage_files) = new_index.stats();
                let invs = config.inventories.len();
                let behaviour = if config.merge { "merge" } else { "replace" };
                *self.index.write().await = new_index;
                *self.config.write().await = config;
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "Ansible Lens: indexed {keys} variables across {files} files; \
                             usages from {usage_files} files; {invs} inventories; \
                             hash_behaviour={behaviour}"
                        ),
                    )
                    .await;
            }
            Err(err) => {
                self.client
                    .log_message(MessageType::ERROR, format!("indexing failed: {err}"))
                    .await;
            }
        }

        // Files opened while the index was still building had their diagnostics
        // computed against an empty index — refresh them now that it's ready.
        let open: Vec<(Url, String)> = self
            .docs
            .read()
            .await
            .iter()
            .map(|(uri, text)| (uri.clone(), text.clone()))
            .collect();
        for (uri, text) in open {
            self.publish_diagnostics(uri, &text).await;
        }

        self.register_file_watchers().await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let mut reload_config = false;
        for change in params.changes {
            // `ansible.cfg` / hosts-file changes invalidate the parsed config.
            reload_config |= is_config_file(&change.uri);
            // Open buffers are handled by document sync; the live text wins.
            if self.docs.read().await.contains_key(&change.uri) {
                continue;
            }
            let Ok(path) = change.uri.to_file_path() else {
                continue;
            };
            match change.typ {
                FileChangeType::DELETED => self.index.write().await.remove(&path),
                _ => {
                    // Created or Changed: pull the new content from disk.
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        self.index.write().await.reindex(&path, &content);
                    }
                }
            }
        }
        if reload_config {
            let roots = self.roots.read().await.clone();
            if let Ok(cfg) = tokio::task::spawn_blocking(move || AnsibleConfig::load(&roots)).await {
                *self.config.write().await = cfg;
            }
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        self.invalidate_pending(&uri).await;
        self.reindex(&uri, &text).await;
        self.publish_diagnostics(uri.clone(), &text).await;
        self.docs.write().await.insert(uri, text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the final change carries the entire document.
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };
        let uri = params.text_document.uri;
        // Store the new text immediately so live queries see it; defer the
        // (workspace-global) reindex until typing pauses.
        self.docs.write().await.insert(uri.clone(), change.text.clone());
        self.schedule_reindex(uri, change.text).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Some(text) = params.text.or(self.text_for(&uri).await) {
            self.invalidate_pending(&uri).await;
            self.reindex(&uri, &text).await;
            self.publish_diagnostics(uri, &text).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.docs.write().await.remove(&uri);
        // Clear diagnostics for the closed document.
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;

        let Some(text) = self.text_for(&uri).await else {
            return Ok(None);
        };
        let Some((line, char_col)) = line_at(&text, pos) else {
            return Ok(None);
        };
        let Some(query) = extract_query(line, char_col) else {
            return Ok(None);
        };

        let defs = {
            let index = self.index.read().await;
            match query {
                Query::Exact(dotted) => index.lookup(&dotted),
                Query::Glob(pattern) => index.lookup_glob(&pattern),
            }
        };
        if defs.is_empty() {
            return Ok(None);
        }

        // Already precedence-sorted (winner first) by the index.
        let locations = defs
            .into_iter()
            .map(|d| Location::new(d.uri, d.range))
            .collect::<Vec<_>>();

        Ok(Some(GotoDefinitionResponse::Array(locations)))
    }

    async fn references(&self, params: ReferenceParams) -> RpcResult<Option<Vec<Location>>> {
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;

        let Some(text) = self.text_for(&uri).await else {
            return Ok(None);
        };

        let Some(target) = resolve_target(&uri, &text, pos) else {
            return Ok(None);
        };

        let index = self.index.read().await;
        let mut locations = index.find_references(&target);
        if params.context.include_declaration {
            locations.extend(
                index
                    .lookup(&target)
                    .into_iter()
                    .map(|d| Location::new(d.uri, d.range)),
            );
        }

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        let pos = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;

        let Some(text) = self.text_for(&uri).await else {
            return Ok(None);
        };
        let Some(target) = resolve_target(&uri, &text, pos) else {
            return Ok(None);
        };

        let defs = self.index.read().await.lookup(&target);
        if defs.is_empty() {
            return Ok(None);
        }

        let config = self.config.read().await;
        // In a host_vars/<host> file we can resolve the way that host sees it;
        // otherwise fall back to the de-opinionated all-sites view.
        let value = match host_context(&uri) {
            Some((inv, host)) if config.inventory(&inv).is_some() => {
                render_resolved_hover(&target, &defs, &inv, &host, &config, config.merge)
            }
            _ => render_table(&target, &defs, &config),
        };

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        }))
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> RpcResult<Option<CompletionResponse>> {
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;

        let Some(text) = self.text_for(&uri).await else {
            return Ok(None);
        };
        let Some((line, col)) = line_at(&text, pos) else {
            return Ok(None);
        };
        if !cursor_in_expr(line, col) {
            return Ok(None);
        }

        let (prefix, partial) = split_dotted_before(line, col);
        let prefix_refs: Vec<&str> = prefix.iter().map(String::as_str).collect();

        // Range covering the partial segment, so accepting replaces it in place.
        let partial_utf16: usize = partial.chars().map(char::len_utf16).sum();
        let edit_range = Range::new(
            Position::new(pos.line, pos.character - partial_utf16 as u32),
            pos,
        );

        let items: Vec<CompletionItem> = self
            .index
            .read()
            .await
            .complete(&prefix_refs, &partial)
            .into_iter()
            .map(|c| completion_item(c, edit_range))
            .collect();

        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(items)))
        }
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }
}

/// Produce undefined-variable diagnostics for a document. Conservative: only a
/// usage whose *root* segment is unknown is flagged, and only after excluding
/// dynamic/magic vars, Jinja keywords, and definitions in this very file.
fn compute_diagnostics(content: &str, index: &VarIndex) -> Vec<Diagnostic> {
    // Roots defined within this file — may not be in the (debounced) index yet,
    // plus Jinja `{% for %}`/`{% set %}` scope variables that are template-local.
    // Parse the document once, then take both views.
    let doc = parse_document(content);
    let mut local: HashSet<String> = HashSet::new();
    local.extend(doc.flat_vars().iter().map(|v| root_of(&v.dotted)));
    local.extend(doc.inline_defs().iter().map(|(_, v)| root_of(&v.dotted)));
    local.extend(jinja_scope_vars(content));

    let mut diags = Vec::new();
    for (i, line) in content.lines().enumerate() {
        for (root, range) in undefined_candidates(line, i as u32) {
            if is_dynamic_var(&root) || local.contains(&root) || index.has_root(&root) {
                continue;
            }
            diags.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("ansible-lens".into()),
                message: format!("Undefined variable `{root}` — no definition found in the workspace"),
                ..Default::default()
            });
        }
    }
    diags
}

fn root_of(dotted: &str) -> String {
    dotted.split('.').next().unwrap_or(dotted).to_string()
}

/// Names that are defined at runtime (not in files) and must never be flagged:
/// Jinja keywords/tests/globals and Ansible magic vars. `ansible_*` facts are
/// covered by a prefix check in [`is_dynamic_var`].
const DYNAMIC_VARS: &[&str] = &[
    // Jinja keywords / operators / constants / block tags
    "if", "else", "elif", "endif", "for", "endfor", "in", "is", "not", "and", "or",
    "true", "false", "none", "True", "False", "None", "set", "endset", "macro",
    "endmacro", "call", "endcall", "filter", "endfilter", "block", "endblock", "with",
    "endwith", "raw", "endraw", "include", "import", "from", "extends", "do",
    // Jinja tests (used bare after `is`)
    "defined", "undefined", "mapping", "sequence", "iterable", "string", "number",
    "boolean", "integer", "float", "sameas", "even", "odd", "divisibleby", "callable",
    // Jinja globals / functions
    "range", "dict", "lipsum", "cycler", "joiner", "namespace", "lookup", "query", "q",
    // Ansible magic vars
    "item", "loop", "omit", "hostvars", "vars", "groups", "group_names",
    "inventory_hostname", "inventory_hostname_short", "play_hosts", "inventory_dir",
    "inventory_file", "playbook_dir", "role_path", "role_name", "role_names",
    "environment",
];

/// A runtime/magic var that must never be flagged: `ansible_*`/`getent_*`,
/// `SCREAMING_CASE` (extra-vars by convention), or a known keyword/magic name.
fn is_dynamic_var(root: &str) -> bool {
    root.starts_with("ansible_")
        || root.starts_with("getent_")
        || is_screaming_case(root)
        || DYNAMIC_VARS.contains(&root)
}

/// `FOO_BAR`, `RELEASE` — has letters, all uppercase. By strong convention
/// these are externally-provided (extra-vars / constants), not file-defined.
fn is_screaming_case(root: &str) -> bool {
    root.chars().any(|c| c.is_alphabetic())
        && !root.chars().any(|c| c.is_lowercase())
}

fn supports_file_watching(params: &InitializeParams) -> bool {
    params
        .capabilities
        .workspace
        .as_ref()
        .and_then(|w| w.did_change_watched_files.as_ref())
        .and_then(|d| d.dynamic_registration)
        .unwrap_or(false)
}

/// Pull workspace roots from `workspace_folders`, falling back to the
/// (deprecated but still common) `root_uri`.
fn collect_roots(params: &InitializeParams) -> Vec<PathBuf> {
    if let Some(folders) = &params.workspace_folders {
        let roots: Vec<PathBuf> = folders
            .iter()
            .filter_map(|f| f.uri.to_file_path().ok())
            .collect();
        if !roots.is_empty() {
            return roots;
        }
    }
    #[allow(deprecated)]
    params
        .root_uri
        .as_ref()
        .and_then(|u| u.to_file_path().ok())
        .into_iter()
        .collect()
}

/// The dotted token left of the cursor split into complete prefix + partial:
/// `foo.ba` -> `(["foo"], "ba")`.
fn split_dotted_before(line: &str, col: usize) -> (Vec<String>, String) {
    let chars: Vec<char> = line.chars().collect();
    let mut start = col.min(chars.len());
    while start > 0 {
        let c = chars[start - 1];
        if c.is_alphanumeric() || c == '_' || c == '.' {
            start -= 1;
        } else {
            break;
        }
    }
    let token: String = chars[start..col.min(chars.len())].iter().collect();
    let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
    let partial = parts.pop().unwrap_or_default();
    (parts, partial)
}

fn completion_item(c: Completion, edit_range: Range) -> CompletionItem {
    let kind = if c.has_children {
        CompletionItemKind::FIELD
    } else {
        CompletionItemKind::VARIABLE
    };

    // Documentation: where the concrete value is defined, or that it's a group.
    let doc = if !c.sources.is_empty() {
        let tiers = c
            .sources
            .iter()
            .map(|s| format!("- {}", s.label()))
            .collect::<Vec<_>>()
            .join("\n");
        format!("`{}`\n\nDefined in:\n{tiers}", c.full_path)
    } else if c.has_children {
        format!("`{}`\n\n(variable group)", c.full_path)
    } else {
        format!("`{}`", c.full_path)
    };

    CompletionItem {
        label: c.segment.clone(),
        kind: Some(kind),
        detail: Some(c.full_path),
        documentation: Some(Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: doc,
        })),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(
            edit_range,
            c.segment,
        ))),
        ..Default::default()
    }
}

/// Does this URI affect the parsed config (ansible.cfg or an inventory hosts file)?
fn is_config_file(uri: &Url) -> bool {
    matches!(
        uri.path().rsplit('/').next(),
        Some("ansible.cfg" | "hosts.yml" | "hosts.yaml" | "hosts" | "inventory")
    )
}

/// Is this URI an Ansible variable file (where keys are definitions)?
fn is_vars_file(uri: &Url) -> bool {
    uri.to_file_path()
        .ok()
        .and_then(|p| VarSource::classify(&p))
        .is_some()
}

/// The line text and its char-column for an LSP position (positions count
/// UTF-16 units; the tokenizer works on `char`s).
fn line_at(text: &str, pos: Position) -> Option<(&str, usize)> {
    let line = text.lines().nth(pos.line as usize)?;
    Some((line, utf16_to_char_idx(line, pos.character as usize)))
}

/// The variable path under the cursor: a YAML key (in a vars file) or a Jinja
/// reference. Shared by references and hover.
fn resolve_target(uri: &Url, text: &str, pos: Position) -> Option<String> {
    let from_key = is_vars_file(uri).then(|| key_path_at(text, pos)).flatten();
    from_key.or_else(|| target_from_usage(text, pos))
}

/// All-sites hover: a table of every definition, the value linking to its source.
fn render_table(target: &str, defs: &[Definition], config: &AnsibleConfig) -> String {
    let mut out = format!("**`{target}`** · _{}_ · {} sites\n\n", value_type(defs), defs.len());
    out.push_str("| Inventory | Tier | Value (→ source) |\n|---|---|---|\n");
    for d in defs {
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            inventory_label(d, config),
            d.source.label(),
            value_link(d),
        ));
    }
    out
}

/// Inventory column label: a real inventory by name, `playbook` for
/// playbook-adjacent group_vars, or `—` for inventory-agnostic sources.
fn inventory_label(d: &Definition, config: &AnsibleConfig) -> String {
    match &d.inventory {
        Some(name) if config.inventory(name).is_some() => format!("`{name}`"),
        Some(_) => "_playbook_".to_string(),
        None => "—".to_string(),
    }
}

/// The value as a clickable link to its defining file and line.
fn value_link(d: &Definition) -> String {
    let line = d.range.start.line + 1; // 1-based for the `#L` anchor
    format!("[{}]({}#L{line})", value_display(d), d.uri)
}

/// Make text table-cell-safe: `|` would split the column and `\|` leaves a
/// stray backslash inside code spans, so substitute a look-alike bar (U+2502).
fn md_cell(s: &str) -> String {
    s.replace('|', "\u{2502}")
}

/// A best-effort value type for the header badge.
fn value_type(defs: &[Definition]) -> &'static str {
    let v = defs.iter().find_map(|d| d.value.as_deref());
    match v {
        None => "dict",
        Some(VAULT_ENCRYPTED) => "vault",
        Some(s) => {
            let t = s.trim();
            if t.is_empty() || t == "~" || t.eq_ignore_ascii_case("null") {
                "null"
            } else if matches!(t, "true" | "false" | "True" | "False" | "yes" | "no") {
                "bool"
            } else if t.parse::<f64>().is_ok() {
                "number"
            } else {
                "string"
            }
        }
    }
}

/// Render a definition's value for display: secret-safe, whitespace-collapsed,
/// middle-truncated (keeps the head and tail of long URLs/paths).
fn value_display(d: &Definition) -> String {
    match d.value.as_deref() {
        Some(VAULT_ENCRYPTED) => "🔒 SECRET (vault)".to_string(),
        Some(v) => format!("`{}`", md_cell(&middle_truncate(&collapse_ws(v), 80))),
        None => "_(dict)_".to_string(),
    }
}

/// Truncate keeping both ends: `https://exam…le.com/path`.
fn middle_truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let head = keep / 2;
    let tail = keep - head;
    format!(
        "{}…{}",
        chars[..head].iter().collect::<String>(),
        chars[chars.len() - tail..].iter().collect::<String>(),
    )
}

/// Hover resolved for a specific host: a table of only the sites that apply to
/// `host` in `inv`, ordered winner-first, with the non-applicable sites and the
/// runtime gap called out in a footer.
fn render_resolved_hover(
    target: &str,
    defs: &[Definition],
    inv: &str,
    host: &str,
    config: &AnsibleConfig,
    merge: bool,
) -> String {
    let mut applicable: Vec<(u32, &Definition)> = Vec::new();
    let mut hidden = 0usize;
    for d in defs {
        match contextual_rank(d, inv, host, config) {
            Some(rank) => applicable.push((rank, d)),
            None => hidden += 1,
        }
    }
    applicable.sort_by(|a, b| b.0.cmp(&a.0)); // highest precedence (winner) first

    let mut out = format!(
        "**`{target}`** — resolved for `{host}` @ `{inv}` · _{}_\n\n",
        value_type(defs)
    );
    if applicable.is_empty() {
        out.push_str("_No definition applies to this host._\n");
        return out;
    }

    out.push_str("| Tier | Value (→ source) |\n|---|---|\n");
    for (_, d) in &applicable {
        out.push_str(&format!("| {} | {} |\n", d.source.label(), value_link(d)));
    }

    let mut notes = vec![if merge {
        "top row wins on conflicts (merge)".to_string()
    } else {
        "top row wins".to_string()
    }];
    if hidden > 0 {
        notes.push(format!("{hidden} site(s) don't apply"));
    }
    notes.push("runtime sources not shown".to_string());
    out.push_str(&format!("\n_{}._\n", notes.join(" · ")));
    out
}

/// Precedence rank of a definition **for a specific host**, or `None` if it
/// doesn't apply (wrong inventory, or a group the host isn't in). Higher wins.
fn contextual_rank(d: &Definition, inv: &str, host: &str, config: &AnsibleConfig) -> Option<u32> {
    use VarSource::*;
    match d.source {
        RoleDefaults => Some(10),
        GroupVarsAll => {
            // inventory `group_vars/all` only counts for the host's own inventory;
            // a playbook-level `group_vars/all` applies everywhere.
            if inventory_scoped(d, config) && d.inventory.as_deref() != Some(inv) {
                None
            } else {
                Some(20)
            }
        }
        GroupVars => {
            if inventory_scoped(d, config) && d.inventory.as_deref() != Some(inv) {
                return None;
            }
            let group = scope_label(&d.uri)?;
            // applies only if `host` is in this group; deeper groups rank higher
            config
                .inventory(inv)?
                .groups_for_host(host)
                .iter()
                .find(|(g, _)| *g == group)
                .map(|(_, depth)| 30 + *depth as u32)
        }
        HostVars => {
            let on_host = d.inventory.as_deref() == Some(inv)
                && scope_label(&d.uri).as_deref() == Some(host);
            on_host.then_some(48)
        }
        PlayVars => Some(50),
        RoleVars => Some(60),
        SetFact | Registered => Some(70),
    }
}

/// Is this definition tied to a real (hosts-file-backed) inventory, as opposed
/// to a playbook-adjacent `group_vars`/`host_vars`?
fn inventory_scoped(d: &Definition, config: &AnsibleConfig) -> bool {
    d.inventory
        .as_deref()
        .is_some_and(|n| config.inventory(n).is_some())
}

/// If `uri` is inside an inventory's `host_vars/<host>/…`, the (inventory, host)
/// context that lets us resolve variables the way they'd apply to that host.
fn host_context(uri: &Url) -> Option<(String, String)> {
    let path = uri.to_file_path().ok()?;
    let comps = path_components(&path);
    let i = comps.iter().position(|&c| c == "host_vars")?;
    let inventory = comps.get(i.checked_sub(1)?)?.to_string();
    let host = strip_yaml_ext(comps.get(i + 1)?).to_string();
    Some((inventory, host))
}

/// Collapse runs of whitespace (incl. newlines) to single spaces.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The host/group name (path element after `group_vars/`/`host_vars/`, ext
/// stripped). `None` for role-level / inline definitions.
fn scope_label(uri: &Url) -> Option<String> {
    let path = uri.to_file_path().ok()?;
    let comps = path_components(&path);
    let i = comps
        .iter()
        .position(|&c| c == "group_vars" || c == "host_vars")?;
    comps.get(i + 1).map(|s| strip_yaml_ext(s).to_string())
}

/// The reference target implied by a Jinja usage under the cursor: the exact
/// path, or the literal prefix of a dynamic-index expression.
fn target_from_usage(text: &str, pos: Position) -> Option<String> {
    let (line, col) = line_at(text, pos)?;
    match extract_query(line, col)? {
        Query::Exact(s) => Some(s),
        Query::Glob(pattern) => leading_literals(&pattern),
    }
}

/// LSP positions count UTF-16 code units; our tokenizer works on `char`s.
/// Convert a UTF-16 column to a char index within `line`.
fn utf16_to_char_idx(line: &str, utf16_col: usize) -> usize {
    let mut units = 0usize;
    for (char_idx, ch) in line.chars().enumerate() {
        if units >= utf16_col {
            return char_idx;
        }
        units += ch.len_utf16();
    }
    line.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precedence::VarSource;

    #[test]
    fn utf16_conversion_is_ascii_identity() {
        assert_eq!(utf16_to_char_idx("percona.password", 8), 8);
    }

    #[test]
    fn utf16_conversion_handles_astral_chars() {
        // "😀" is 2 UTF-16 units, 1 char. A column of 2 lands on the next char.
        let line = "😀x";
        assert_eq!(utf16_to_char_idx(line, 2), 1);
    }

    #[test]
    fn scope_label_extracts_host_and_group_names() {
        let url = |p: &str| Url::from_file_path(p).unwrap();
        // directory form: host_vars/<host>/file.yml
        assert_eq!(
            scope_label(&url("/ws/inv_prod/host_vars/web1/cpsd.yml")),
            Some("web1".to_string())
        );
        // file form: group_vars/all.yml -> extension stripped
        assert_eq!(
            scope_label(&url("/ws/group_vars/all.yml")),
            Some("all".to_string())
        );
        // role defaults are inventory/scope-independent
        assert_eq!(scope_label(&url("/ws/roles/x/defaults/main.yml")), None);
    }

    fn def(value: Option<&str>) -> Definition {
        Definition {
            uri: Url::from_file_path("/ws/group_vars/all.yml").unwrap(),
            range: Range::default(),
            source: VarSource::GroupVarsAll,
            inventory: None,
            value: value.map(String::from),
        }
    }

    #[test]
    fn value_display_marks_vault_as_secret_not_ciphertext() {
        let v = value_display(&def(Some(VAULT_ENCRYPTED)));
        assert!(v.contains("SECRET"));
        assert!(!v.contains(VAULT_ENCRYPTED)); // never leak the raw sentinel/ciphertext
    }

    #[test]
    fn value_display_collapses_multiline_values() {
        let v = value_display(&def(Some("line one\nline two")));
        assert!(v.contains("line one line two"));
        assert!(!v.contains("line one\nline two"));
    }

    #[test]
    fn value_display_neutralizes_table_pipes() {
        // Jinja filter values are full of `|` — no raw pipe may survive (it would
        // break the table); they become a look-alike that renders cleanly.
        let v = value_display(&def(Some("{{ inventory_dir | basename | default('x') }}")));
        assert!(!v.contains('|'), "raw pipe would break the table: {v}");
        assert!(v.contains('\u{2502}'));
    }
}
