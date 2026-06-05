//! In-memory workspace index.
//!
//! Definitions live in a segment trie (`DefNode`): lookup/glob/completion are
//! tree descents, never linear key scans. Usages are held per-file (for cheap
//! incremental removal) and bucketed by root segment (so "find references"
//! scans only the matching bucket). Per-file source maps let one file be
//! re-indexed in time proportional to its own size.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::{Location, Range, Url};
use walkdir::WalkDir;

use crate::flatten::{DefKind, FlatVar, parse_document};
use crate::jinja::{GlobSeg, usages_in_line};
use crate::precedence::{VarSource, has_yaml_ext};

fn kind_source(kind: DefKind) -> VarSource {
    match kind {
        DefKind::Vars | DefKind::Loop => VarSource::PlayVars,
        DefKind::SetFact => VarSource::SetFact,
        DefKind::Register => VarSource::Registered,
    }
}

#[derive(Debug, Clone)]
pub struct Definition {
    pub uri: Url,
    pub range: Range,
    pub source: VarSource,
    pub inventory: Option<String>,
    pub value: Option<String>,
}

/// One trie node: a path segment, the defs ending here, and child segments.
#[derive(Default)]
struct DefNode {
    children: BTreeMap<String, DefNode>,
    defs: Vec<Definition>,
}

#[derive(Debug, Clone)]
struct Usage {
    pattern: Vec<GlobSeg>,
    range: Range,
}

#[derive(Debug, Clone)]
struct UsageRef {
    uri: Url,
    pattern: Vec<GlobSeg>,
    range: Range,
}

#[derive(Default)]
pub struct VarIndex {
    // Query structures (derived):
    tree: DefNode,
    usages_by_root: HashMap<String, Vec<UsageRef>>,
    // Source of truth (per file, for incremental removal):
    file_defs: HashMap<Url, Vec<String>>,
    file_usages: HashMap<Url, Vec<Usage>>,
    files: usize,
}

impl VarIndex {
    /// Build the index by scanning each workspace root once. Every candidate
    /// file contributes usages; Ansible variable files also contribute defs.
    pub fn build(roots: &[PathBuf]) -> Self {
        let mut index = VarIndex::default();
        for root in roots {
            for entry in WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| !is_pruned_dir(e.path()))
                .filter_map(Result::ok)
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if !is_usage_candidate(path) && VarSource::classify(path).is_none() {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(path) else {
                    continue;
                };
                index.ingest(path, &content);
            }
        }
        index
    }

    /// (Re)index one file, replacing everything previously derived from it. Runs
    /// in time proportional to the file's own contributions, not the workspace.
    pub fn reindex(&mut self, path: &Path, content: &str) {
        let Ok(uri) = Url::from_file_path(path) else {
            return;
        };
        self.remove_uri(&uri);
        self.ingest(path, content);
    }

    /// Drop everything derived from a file (it was deleted on disk).
    pub fn remove(&mut self, path: &Path) {
        if let Ok(uri) = Url::from_file_path(path) {
            self.remove_uri(&uri);
        }
    }

    /// Add a file's definitions (if it's a vars file) and usages (if relevant).
    fn ingest(&mut self, path: &Path, content: &str) {
        let Ok(uri) = Url::from_file_path(path) else {
            return;
        };

        // Parse once: wholesale keys (vars files only) + inline defs (any file).
        let doc = parse_document(content);
        let mut defs: Vec<(VarSource, FlatVar)> = Vec::new();
        if let Some(source) = VarSource::classify(path) {
            defs.extend(doc.flat_vars().into_iter().map(|v| (source, v)));
        }
        defs.extend(
            doc.inline_defs()
                .into_iter()
                .map(|(kind, v)| (kind_source(kind), v)),
        );

        if !defs.is_empty() {
            self.files += 1;
            let inventory = crate::precedence::inventory_of(path);
            let mut paths = Vec::with_capacity(defs.len());
            for (source, var) in defs {
                insert_def(
                    &mut self.tree,
                    &var.dotted,
                    Definition {
                        uri: uri.clone(),
                        range: var.range,
                        source,
                        inventory: inventory.clone(),
                        value: var.value,
                    },
                );
                paths.push(var.dotted);
            }
            self.file_defs.insert(uri.clone(), paths);
        }

        if is_usage_candidate(path) {
            let usages: Vec<Usage> = content
                .lines()
                .enumerate()
                .flat_map(|(i, line)| usages_in_line(line, i as u32))
                .map(|(pattern, range)| Usage { pattern, range })
                .collect();
            if !usages.is_empty() {
                for u in &usages {
                    if let Some(GlobSeg::Lit(root)) = u.pattern.first() {
                        self.usages_by_root
                            .entry(root.clone())
                            .or_default()
                            .push(UsageRef {
                                uri: uri.clone(),
                                pattern: u.pattern.clone(),
                                range: u.range,
                            });
                    }
                }
                self.file_usages.insert(uri, usages);
            }
        }
    }

    fn remove_uri(&mut self, uri: &Url) {
        if let Some(paths) = self.file_defs.remove(uri) {
            self.files = self.files.saturating_sub(1);
            for dotted in paths {
                let segs: Vec<&str> = dotted.split('.').collect();
                remove_def_at(&mut self.tree, &segs, 0, uri);
            }
        }
        if let Some(usages) = self.file_usages.remove(uri) {
            for u in usages {
                if let Some(GlobSeg::Lit(root)) = u.pattern.first()
                    && let Some(bucket) = self.usages_by_root.get_mut(root)
                {
                    bucket.retain(|r| &r.uri != uri);
                    if bucket.is_empty() {
                        self.usages_by_root.remove(root);
                    }
                }
            }
        }
    }

    /// All definitions of `dotted`, ordered by precedence (winner first).
    pub fn lookup(&self, dotted: &str) -> Vec<Definition> {
        let mut node = &self.tree;
        for seg in dotted.split('.') {
            match node.children.get(seg) {
                Some(child) => node = child,
                None => return Vec::new(),
            }
        }
        let mut defs = node.defs.clone();
        sort_by_precedence(&mut defs);
        defs
    }

    /// All definitions whose key matches a glob pattern (each `Wild` matches one
    /// segment), e.g. `a.b.*.d`. Precedence-ordered.
    pub fn lookup_glob(&self, pattern: &[GlobSeg]) -> Vec<Definition> {
        let mut defs = Vec::new();
        glob_collect(&self.tree, pattern, &mut defs);
        sort_by_precedence(&mut defs);
        defs
    }

    /// Every usage referencing `target` (prefix-glob: at least as deep, leading
    /// segments match, `Wild` matches anything). Scans only the root's bucket.
    pub fn find_references(&self, target: &str) -> Vec<Location> {
        let target: Vec<&str> = target.split('.').collect();
        let Some(root) = target.first() else {
            return Vec::new();
        };
        self.usages_by_root
            .get(*root)
            .into_iter()
            .flatten()
            .filter(|r| usage_references(&r.pattern, &target))
            .map(|r| Location::new(r.uri.clone(), r.range))
            .collect()
    }

    /// Completion candidates for the next segment after `prefix` (filtered by
    /// the `partial` being typed): one entry per distinct child of that node.
    pub fn complete(&self, prefix: &[&str], partial: &str) -> Vec<Completion> {
        let mut node = &self.tree;
        for seg in prefix {
            match node.children.get(*seg) {
                Some(child) => node = child,
                None => return Vec::new(),
            }
        }
        node.children
            .range(partial.to_string()..)
            .take_while(|(seg, _)| seg.starts_with(partial))
            .map(|(seg, child)| {
                let mut sources: Vec<VarSource> = child
                    .defs
                    .iter()
                    .map(|d| d.source)
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect();
                sources.sort_by_key(|s| std::cmp::Reverse(s.precedence()));
                Completion {
                    full_path: prefixed(prefix, seg),
                    segment: seg.clone(),
                    has_children: !child.children.is_empty(),
                    sources,
                }
            })
            .collect()
    }

    pub fn has_root(&self, root: &str) -> bool {
        self.tree.children.contains_key(root)
    }

    /// (#def files, #distinct keys, #usage files) — logged after indexing.
    pub fn stats(&self) -> (usize, usize, usize) {
        (self.files, count_keys(&self.tree), self.file_usages.len())
    }
}

/// A completion candidate (one path segment).
pub struct Completion {
    pub segment: String,
    pub full_path: String,
    pub has_children: bool,
    /// Defining tiers, precedence-ordered (empty for pure dicts).
    pub sources: Vec<VarSource>,
}

/// Insert a definition at `dotted`, creating intermediate nodes as needed.
fn insert_def(tree: &mut DefNode, dotted: &str, def: Definition) {
    let mut node = tree;
    for seg in dotted.split('.') {
        node = node.children.entry(seg.to_string()).or_default();
    }
    node.defs.push(def);
}

/// Remove a file's definitions at one path, pruning nodes that become empty.
/// Returns whether `node` is empty (no defs, no children) afterwards.
fn remove_def_at(node: &mut DefNode, segs: &[&str], idx: usize, uri: &Url) -> bool {
    match segs.get(idx) {
        None => node.defs.retain(|d| &d.uri != uri),
        Some(seg) => {
            if let Some(child) = node.children.get_mut(*seg)
                && remove_def_at(child, segs, idx + 1, uri)
            {
                node.children.remove(*seg);
            }
        }
    }
    node.defs.is_empty() && node.children.is_empty()
}

/// Collect defs matching a glob pattern by descending the trie; `Wild` branches
/// into every child.
fn glob_collect(node: &DefNode, pattern: &[GlobSeg], out: &mut Vec<Definition>) {
    match pattern.split_first() {
        None => out.extend(node.defs.iter().cloned()),
        Some((GlobSeg::Lit(seg), rest)) => {
            if let Some(child) = node.children.get(seg) {
                glob_collect(child, rest, out);
            }
        }
        Some((GlobSeg::Wild, rest)) => {
            for child in node.children.values() {
                glob_collect(child, rest, out);
            }
        }
    }
}

/// Count nodes that hold at least one definition (distinct defined paths).
fn count_keys(node: &DefNode) -> usize {
    let here = usize::from(!node.defs.is_empty());
    here + node.children.values().map(count_keys).sum::<usize>()
}

/// Higher precedence first; stable so same-tier files keep insertion order.
fn sort_by_precedence(defs: &mut [Definition]) {
    defs.sort_by(|a, b| b.source.precedence().cmp(&a.source.precedence()));
}

/// Prefix-glob rule for references: `usage` references `target` when it is at
/// least as deep and its first `target.len()` segments match.
fn usage_references(usage: &[GlobSeg], target: &[&str]) -> bool {
    usage.len() >= target.len()
        && target.iter().enumerate().all(|(i, t)| match &usage[i] {
            GlobSeg::Lit(lit) => lit == t,
            GlobSeg::Wild => true,
        })
}

fn prefixed(prefix: &[&str], seg: &str) -> String {
    if prefix.is_empty() {
        seg.to_string()
    } else {
        format!("{}.{}", prefix.join("."), seg)
    }
}

/// Files scanned for *usages*: all YAML, plus anything under a `templates/` dir.
fn is_usage_candidate(path: &Path) -> bool {
    has_yaml_ext(path)
        || path
            .components()
            .any(|c| c.as_os_str().to_str() == Some("templates"))
}

fn is_pruned_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(
            ".git" | ".svn" | ".hg" | "node_modules" | ".venv" | "venv" | "__pycache__" | "target"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Position;

    fn rng() -> Range {
        Range::new(Position::new(0, 0), Position::new(0, 1))
    }

    impl VarIndex {
        fn add_def(&mut self, uri: &str, dotted: &str, source: VarSource) {
            let uri = Url::parse(uri).unwrap();
            insert_def(
                &mut self.tree,
                dotted,
                Definition {
                    uri: uri.clone(),
                    range: rng(),
                    source,
                    inventory: None,
                    value: None,
                },
            );
            self.file_defs.entry(uri).or_default().push(dotted.into());
        }
        fn add_usage(&mut self, uri: &str, segs: &[GlobSeg]) {
            let uri = Url::parse(uri).unwrap();
            if let Some(GlobSeg::Lit(root)) = segs.first() {
                self.usages_by_root
                    .entry(root.clone())
                    .or_default()
                    .push(UsageRef {
                        uri,
                        pattern: segs.to_vec(),
                        range: rng(),
                    });
            }
        }
    }

    fn lit(s: &str) -> GlobSeg {
        GlobSeg::Lit(s.into())
    }

    #[test]
    fn lookup_orders_by_precedence_winner_first() {
        let mut index = VarIndex::default();
        index.add_def(
            "file:///defaults.yml",
            "app.db.password",
            VarSource::RoleDefaults,
        );
        index.add_def(
            "file:///hostvars.yml",
            "app.db.password",
            VarSource::HostVars,
        );
        index.add_def(
            "file:///groupvars.yml",
            "app.db.password",
            VarSource::GroupVars,
        );

        let order: Vec<_> = index
            .lookup("app.db.password")
            .iter()
            .map(|d| d.source)
            .collect();
        assert_eq!(
            order,
            vec![
                VarSource::HostVars,
                VarSource::GroupVars,
                VarSource::RoleDefaults
            ]
        );
    }

    #[test]
    fn unknown_key_returns_empty() {
        assert!(VarIndex::default().lookup("nope").is_empty());
    }

    #[test]
    fn glob_lookup_branches_on_wild() {
        let mut index = VarIndex::default();
        index.add_def(
            "file:///v.yml",
            "data.servers.prod.address",
            VarSource::GroupVars,
        );
        index.add_def(
            "file:///v.yml",
            "data.servers.staging.address",
            VarSource::GroupVars,
        );
        index.add_def(
            "file:///v.yml",
            "data.servers.prod.other",
            VarSource::GroupVars,
        );

        let pattern = vec![lit("data"), lit("servers"), GlobSeg::Wild, lit("address")];
        assert_eq!(index.lookup_glob(&pattern).len(), 2);
    }

    #[test]
    fn completion_offers_children_and_marks_dicts() {
        let mut index = VarIndex::default();
        for key in [
            "app.install_dir",
            "app.bind_host",
            "app.keycloak.realm",
            "region",
        ] {
            index.add_def("file:///g.yml", key, VarSource::GroupVars);
        }

        let top: Vec<_> = index
            .complete(&[], "ap")
            .into_iter()
            .map(|c| (c.segment, c.has_children))
            .collect();
        assert_eq!(top, vec![("app".to_string(), true)]);

        let members: Vec<_> = index
            .complete(&["app"], "")
            .into_iter()
            .map(|c| (c.segment, c.has_children))
            .collect();
        assert_eq!(
            members,
            vec![
                ("bind_host".to_string(), false),
                ("install_dir".to_string(), false),
                ("keycloak".to_string(), true),
            ]
        );
    }

    #[test]
    fn references_exact_parent_and_dynamic() {
        let mut index = VarIndex::default();
        index.add_usage("file:///t", &[lit("app"), lit("install_dir")]);
        index.add_usage("file:///t", &[lit("app"), lit("bind_host")]);
        index.add_usage(
            "file:///t",
            &[lit("data"), lit("servers"), GlobSeg::Wild, lit("address")],
        );
        index.add_usage("file:///t", &[lit("unrelated")]);

        assert_eq!(index.find_references("app.install_dir").len(), 1);
        assert_eq!(index.find_references("app").len(), 2);
        assert_eq!(index.find_references("data.servers.prod.address").len(), 1);
        assert!(index.find_references("unrelated.deeper.leaf").is_empty());
    }

    #[test]
    fn reindex_removes_stale_defs_and_prunes() {
        let mut index = VarIndex::default();
        let path = std::path::Path::new("/ws/group_vars/all.yml");
        index.reindex(path, "console_ui:\n  install_dir: /x\n  bind_host: y\n");
        assert_eq!(index.lookup("console_ui.install_dir").len(), 1);

        // Re-index the same file with bind_host removed -> stale def is gone.
        index.reindex(path, "console_ui:\n  install_dir: /x\n");
        assert!(index.lookup("console_ui.bind_host").is_empty());
        assert_eq!(index.lookup("console_ui.install_dir").len(), 1);
        // completion no longer offers the pruned child
        let members: Vec<_> = index
            .complete(&["console_ui"], "")
            .into_iter()
            .map(|c| c.segment)
            .collect();
        assert_eq!(members, vec!["install_dir".to_string()]);
    }
}
