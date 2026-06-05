//! Classify a variable file into a source kind and rank it by Ansible's
//! documented precedence (the file-based subset a static scan can see):
//!
//!   role defaults < group_vars/all < group_vars/<group> < host_vars < role vars
//!
//! Note `roles/*/vars/main.yml` ("role vars") outranks host_vars — surprising,
//! but it's the documented order.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VarSource {
    RoleDefaults,
    GroupVarsAll,
    GroupVars,
    HostVars,
    RoleVars,
    PlayVars,
    SetFact,
    Registered,
}

impl VarSource {
    /// Higher wins; gaps left so finer tiers can slot in without renumbering.
    pub fn precedence(self) -> u32 {
        match self {
            VarSource::RoleDefaults => 10,
            VarSource::GroupVarsAll => 20,
            VarSource::GroupVars => 30,
            VarSource::HostVars => 40,
            VarSource::PlayVars => 50,
            VarSource::RoleVars => 60,
            VarSource::SetFact | VarSource::Registered => 70,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            VarSource::RoleDefaults => "role defaults",
            VarSource::GroupVarsAll => "group_vars/all",
            VarSource::GroupVars => "group_vars",
            VarSource::HostVars => "host_vars",
            VarSource::RoleVars => "role vars",
            VarSource::PlayVars => "vars",
            VarSource::SetFact => "set_fact",
            VarSource::Registered => "registered",
        }
    }

    /// Classify a path as an Ansible variable file (`None` if it isn't one).
    /// Handles both the file form (`group_vars/web.yml`) and the directory form
    /// (`group_vars/web/foo.yml`), plus role `defaults/` and `vars/`.
    pub fn classify(path: &Path) -> Option<VarSource> {
        if !has_yaml_ext(path) {
            return None;
        }
        let comps = path_components(path);

        // roles/<role>/defaults/...  and  roles/<role>/vars/...
        for i in 0..comps.len() {
            if comps[i] == "roles" && i + 3 < comps.len() {
                match comps[i + 2] {
                    "defaults" => return Some(VarSource::RoleDefaults),
                    "vars" => return Some(VarSource::RoleVars),
                    _ => {}
                }
            }
        }

        // group_vars/... and host_vars/...
        for i in 0..comps.len() {
            match comps[i] {
                "group_vars" if i + 1 < comps.len() => {
                    // `all` (file or dir) is the special group; anything else is a group.
                    let next = strip_yaml_ext(comps[i + 1]);
                    return Some(if next == "all" {
                        VarSource::GroupVarsAll
                    } else {
                        VarSource::GroupVars
                    });
                }
                "host_vars" if i + 1 < comps.len() => return Some(VarSource::HostVars),
                _ => {}
            }
        }

        None
    }
}

/// The inventory a variable file belongs to: the dir holding its
/// `group_vars/`/`host_vars/` (e.g. `prod`). `None` if not under one.
pub fn inventory_of(path: &Path) -> Option<String> {
    let comps = path_components(path);
    comps
        .iter()
        .position(|&c| c == "group_vars" || c == "host_vars")
        .filter(|&i| i > 0)
        .map(|i| comps[i - 1].to_string())
}

/// String-ified path components (segments that aren't valid UTF-8 are skipped).
pub fn path_components(path: &Path) -> Vec<&str> {
    path.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect()
}

pub fn has_yaml_ext(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yml") | Some("yaml")
    )
}

pub fn strip_yaml_ext(name: &str) -> &str {
    name.strip_suffix(".yml")
        .or_else(|| name.strip_suffix(".yaml"))
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn classify(p: &str) -> Option<VarSource> {
        VarSource::classify(&PathBuf::from(p))
    }

    #[test]
    fn recognises_each_source() {
        assert_eq!(classify("group_vars/all.yml"), Some(VarSource::GroupVarsAll));
        assert_eq!(classify("group_vars/all/x.yml"), Some(VarSource::GroupVarsAll));
        assert_eq!(classify("group_vars/db.yml"), Some(VarSource::GroupVars));
        assert_eq!(classify("group_vars/db/main.yaml"), Some(VarSource::GroupVars));
        assert_eq!(classify("host_vars/web1.yml"), Some(VarSource::HostVars));
        assert_eq!(
            classify("roles/myrole/defaults/main.yml"),
            Some(VarSource::RoleDefaults)
        );
        assert_eq!(
            classify("roles/myrole/vars/main.yml"),
            Some(VarSource::RoleVars)
        );
    }

    #[test]
    fn ignores_irrelevant_files() {
        assert_eq!(classify("README.md"), None);
        assert_eq!(classify("playbook.yml"), None);
        assert_eq!(classify("roles/myrole/tasks/main.yml"), None);
        assert_eq!(classify("group_vars"), None);
    }

    #[test]
    fn precedence_orders_correctly() {
        assert!(VarSource::RoleVars.precedence() > VarSource::HostVars.precedence());
        assert!(VarSource::HostVars.precedence() > VarSource::GroupVars.precedence());
        assert!(VarSource::GroupVars.precedence() > VarSource::GroupVarsAll.precedence());
        assert!(VarSource::GroupVarsAll.precedence() > VarSource::RoleDefaults.precedence());
    }
}
