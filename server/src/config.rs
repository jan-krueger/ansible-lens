//! Project config: `ansible.cfg` (`hash_behaviour`) and parsed inventory group
//! graphs, so ordering/merge match the project's setup. Static picture only —
//! no runtime inputs (active `-i` inventory, extra-vars, facts, `set_fact`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use marked_yaml::Node;
use marked_yaml::parse_yaml;
use marked_yaml::types::MarkedMappingNode;
use walkdir::WalkDir;

#[derive(Default)]
pub struct AnsibleConfig {
    /// `hash_behaviour = merge` (dicts combine) vs the default `replace`.
    pub merge: bool,
    pub inventories: Vec<Inventory>,
}

pub struct Inventory {
    pub name: String,
    /// host -> its groups, shallow→deep with depth (depth drives group_vars order).
    host_groups: HashMap<String, Vec<(String, usize)>>,
}

#[derive(Default)]
struct Group {
    children: Vec<String>,
    hosts: Vec<String>,
}

impl AnsibleConfig {
    /// Scan the workspace roots for `ansible.cfg` and inventory hosts files.
    pub fn load(roots: &[PathBuf]) -> Self {
        let mut merge = false;
        let mut inventory_roots: HashSet<PathBuf> = HashSet::new();

        for root in roots {
            for entry in WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| !is_pruned(e.path()))
                .filter_map(Result::ok)
            {
                let path = entry.path();
                if entry.file_type().is_file() && path.file_name() == Some("ansible.cfg".as_ref()) {
                    merge |= parse_hash_behaviour_merge(path);
                }
                // An inventory root is the parent of a `group_vars`/`host_vars` dir.
                if entry.file_type().is_dir() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if (name == "group_vars" || name == "host_vars") && path.parent().is_some()
                        {
                            inventory_roots.insert(path.parent().unwrap().to_path_buf());
                        }
                    }
                }
            }
        }

        let inventories = inventory_roots
            .iter()
            .filter_map(|r| Inventory::load(r))
            .collect();

        AnsibleConfig { merge, inventories }
    }

    pub fn inventory(&self, name: &str) -> Option<&Inventory> {
        self.inventories.iter().find(|i| i.name == name)
    }
}

impl Inventory {
    /// Load an inventory from `root` if it contains a parseable hosts file.
    fn load(root: &Path) -> Option<Inventory> {
        let content = find_hosts_file(root).and_then(|p| std::fs::read_to_string(p).ok())?;
        let groups = parse_yaml_inventory(&content);
        if groups.is_empty() {
            return None;
        }

        // child -> parents, for walking a host up to its ancestor groups.
        let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
        for (g, info) in &groups {
            for c in &info.children {
                parents.entry(c).or_default().push(g);
            }
        }

        let mut depth_memo: HashMap<String, usize> = HashMap::new();
        for g in groups.keys() {
            depth(g, &parents, &mut depth_memo, &mut HashSet::new());
        }

        let all_hosts: HashSet<&str> = groups
            .values()
            .flat_map(|g| g.hosts.iter().map(String::as_str))
            .collect();

        let mut host_groups = HashMap::new();
        for host in all_hosts {
            let mut seen: HashSet<&str> = HashSet::new();
            let mut stack: Vec<&str> = groups
                .iter()
                .filter(|(_, g)| g.hosts.iter().any(|h| h == host))
                .map(|(n, _)| n.as_str())
                .collect();
            while let Some(g) = stack.pop() {
                if !seen.insert(g) {
                    continue;
                }
                if let Some(ps) = parents.get(g) {
                    stack.extend(ps);
                }
            }
            let mut list: Vec<(String, usize)> = seen
                .into_iter()
                .map(|g| (g.to_string(), depth_memo.get(g).copied().unwrap_or(1)))
                .collect();
            list.push(("all".to_string(), 0));
            list.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            host_groups.insert(host.to_string(), list);
        }

        Some(Inventory {
            name: root.file_name()?.to_str()?.to_string(),
            host_groups,
        })
    }

    /// The host's groups, shallow→deep with depth (`all` first, overriders last).
    pub fn groups_for_host(&self, host: &str) -> &[(String, usize)] {
        self.host_groups.get(host).map_or(&[], Vec::as_slice)
    }
}

/// Read `hash_behaviour`/`hash_behavior` from the `[defaults]` section.
fn parse_hash_behaviour_merge(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let mut in_defaults = false;
    for line in content.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[') {
            in_defaults = section.trim_end_matches(']').trim() == "defaults";
        } else if in_defaults {
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                if k == "hash_behaviour" || k == "hash_behavior" {
                    return v.trim() == "merge";
                }
            }
        }
    }
    false
}

fn find_hosts_file(root: &Path) -> Option<PathBuf> {
    ["hosts.yml", "hosts.yaml", "hosts", "inventory"]
        .iter()
        .map(|name| root.join(name))
        .find(|p| p.is_file())
}

/// Parse a YAML inventory into a flat `group -> {children, hosts}` map. Handles
/// both the flat form (groups at top level) and the nested `all:`/`children:`
/// form. (INI inventories aren't parsed yet — they yield an empty map.)
fn parse_yaml_inventory(content: &str) -> HashMap<String, Group> {
    let mut groups = HashMap::new();
    if let Ok(root) = parse_yaml(0, content) {
        if let Some(map) = root.as_mapping() {
            for (name, body) in map.iter() {
                collect_group(name.as_str(), body, &mut groups);
            }
        }
    }
    groups
}

fn collect_group(name: &str, body: &Node, groups: &mut HashMap<String, Group>) {
    let mut children = Vec::new();
    let mut hosts = Vec::new();
    if let Some(map) = body.as_mapping() {
        if let Some(cmap) = child(map, "children").and_then(Node::as_mapping) {
            for (cname, cbody) in cmap.iter() {
                children.push(cname.as_str().to_string());
                collect_group(cname.as_str(), cbody, groups); // inline (nested form)
            }
        }
        if let Some(hmap) = child(map, "hosts").and_then(Node::as_mapping) {
            for (hname, _) in hmap.iter() {
                hosts.push(hname.as_str().to_string());
            }
        }
    }
    let g = groups.entry(name.to_string()).or_default();
    g.children.extend(children);
    g.hosts.extend(hosts);
}

fn child<'a>(map: &'a MarkedMappingNode, key: &str) -> Option<&'a Node> {
    map.iter().find(|(k, _)| k.as_str() == key).map(|(_, v)| v)
}

/// Longest path from a root group (groups with no parents are depth 1, `all` is
/// conceptually depth 0). Memoized, with a cycle guard.
fn depth(
    group: &str,
    parents: &HashMap<&str, Vec<&str>>,
    memo: &mut HashMap<String, usize>,
    visiting: &mut HashSet<String>,
) -> usize {
    if let Some(&d) = memo.get(group) {
        return d;
    }
    if !visiting.insert(group.to_string()) {
        return 1; // cycle — bail with a neutral depth
    }
    let d = match parents.get(group) {
        Some(ps) if !ps.is_empty() => {
            1 + ps
                .iter()
                .map(|p| depth(p, parents, memo, visiting))
                .max()
                .unwrap_or(0)
        }
        _ => 1,
    };
    visiting.remove(group);
    memo.insert(group.to_string(), d);
    d
}

fn is_pruned(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(".git" | "node_modules" | ".venv" | "venv" | "target" | "__pycache__")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_group_hierarchy_and_host_membership() {
        // prod -> {web, db}
        let yaml = "\
prod:
  children:
    web:
    db:
web:
  hosts:
    web1:
db:
  hosts:
    db1:
    web1:
";
        let groups = parse_yaml_inventory(yaml);
        assert!(groups.contains_key("prod"));
        assert_eq!(groups["prod"].children, vec!["web", "db"]);
        assert!(groups["web"].hosts.contains(&"web1".to_string()));
    }

    #[test]
    fn groups_for_host_ordered_by_depth() {
        let yaml = "\
prod:
  children:
    web:
    db:
web:
  hosts:
    web1:
db:
  hosts:
    web1:
";
        let groups = parse_yaml_inventory(yaml);
        let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
        for (g, info) in &groups {
            for c in &info.children {
                parents.entry(c).or_default().push(g);
            }
        }
        let mut memo = HashMap::new();
        // web1 is in web + db (depth 2), under prod (depth 1), all (0)
        assert_eq!(depth("prod", &parents, &mut memo, &mut HashSet::new()), 1);
        assert_eq!(depth("web", &parents, &mut memo, &mut HashSet::new()), 2);
    }
}
