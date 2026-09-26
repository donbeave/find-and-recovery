//! Planning logic for exact duplicate remote recovery branches.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteBranch {
    pub name: String,
    pub oid: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryRef {
    pub name: String,
    pub managed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateGroup {
    pub oid: String,
    pub branches: Vec<String>,
    pub delete_candidates: Vec<String>,
}

/// Delete candidates must be managed recovery refs with the same exact tip.
/// Unmanaged refs and protected branches can never become delete candidates.
pub fn exact_duplicate_groups(
    branches: impl IntoIterator<Item = RemoteBranch>,
    recovery_refs: impl IntoIterator<Item = RecoveryRef>,
    protected_refs: &BTreeSet<String>,
) -> Vec<DuplicateGroup> {
    let managed = recovery_refs
        .into_iter()
        .filter(|recovery_ref| recovery_ref.managed)
        .map(|recovery_ref| recovery_ref.name)
        .collect::<BTreeSet<_>>();
    let mut by_oid: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for branch in branches {
        by_oid.entry(branch.oid).or_default().insert(branch.name);
    }
    by_oid
        .into_iter()
        .filter_map(|(oid, names)| {
            if names.len() < 2 {
                return None;
            }
            let branches = names.into_iter().collect::<Vec<_>>();
            let keep = branches
                .iter()
                .find(|name| protected_refs.contains(*name))
                .cloned()
                .or_else(|| {
                    branches
                        .iter()
                        .find(|name| !managed.contains(*name))
                        .cloned()
                })
                .or_else(|| branches.first().cloned())?;
            let delete_candidates = branches
                .iter()
                .filter(|name| {
                    name.as_str() != keep
                        && managed.contains(*name)
                        && !protected_refs.contains(*name)
                })
                .cloned()
                .collect();
            Some(DuplicateGroup {
                oid,
                branches,
                delete_candidates,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(name: &str, oid: &str) -> RemoteBranch {
        RemoteBranch {
            name: name.into(),
            oid: oid.into(),
        }
    }

    fn managed(names: &[&str]) -> Vec<RecoveryRef> {
        names
            .iter()
            .map(|name| RecoveryRef {
                name: (*name).into(),
                managed: true,
            })
            .collect()
    }

    #[test]
    fn identical_commit_ids_are_duplicates() {
        let groups = exact_duplicate_groups(
            [branch("feature/a", "same"), branch("recovery/b", "same")],
            managed(&["feature/a", "recovery/b"]),
            &BTreeSet::new(),
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].delete_candidates, ["recovery/b"]);
    }

    #[test]
    fn different_commit_ids_are_not_duplicates_even_with_same_content() {
        assert!(
            exact_duplicate_groups(
                [branch("feature/a", "one"), branch("feature/b", "two")],
                managed(&["feature/a", "feature/b"]),
                &BTreeSet::new(),
            )
            .is_empty()
        );
    }

    #[test]
    fn never_deletes_unmanaged_remote_branch() {
        let groups = exact_duplicate_groups(
            [
                branch("existing/topic", "same"),
                branch("recovery/new", "same"),
            ],
            managed(&["recovery/new"]),
            &BTreeSet::new(),
        );
        assert_eq!(groups[0].delete_candidates, ["recovery/new"]);
    }

    #[test]
    fn never_deletes_main_branch() {
        let groups = exact_duplicate_groups(
            [branch("main", "same"), branch("recovery/new", "same")],
            managed(&["main", "recovery/new"]),
            &["main".into()].into_iter().collect(),
        );
        assert_eq!(groups[0].delete_candidates, ["recovery/new"]);
    }
}
