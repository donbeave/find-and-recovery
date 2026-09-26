//! Planning for duplicate remote branches.

use std::collections::{BTreeMap, BTreeSet};

/// Remote branch facts collected from one stable snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchSnapshot {
    pub name: String,
    pub oid: String,
    pub tree_oid: String,
    pub commit_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactDuplicateGroup {
    pub oid: String,
    pub branches: Vec<String>,
    pub keeper: String,
    pub delete_candidates: Vec<String>,
}

/// Plan deletion only for exact commit aliases. Names in `protected` can be
/// keepers but never candidates; callers should protect every ref not owned
/// by this run.
pub fn exact_duplicate_groups(
    branches: impl IntoIterator<Item = BranchSnapshot>,
    protected: &BTreeSet<String>,
) -> Vec<ExactDuplicateGroup> {
    let mut by_oid = BTreeMap::<String, BTreeSet<String>>::new();
    for branch in branches {
        by_oid.entry(branch.oid).or_default().insert(branch.name);
    }
    by_oid
        .into_iter()
        .filter_map(|(oid, names)| {
            if names.len() < 2 {
                return None;
            }
            let keeper = names
                .iter()
                .find(|name| protected.contains(*name))
                .or_else(|| names.iter().next())?
                .clone();
            Some(ExactDuplicateGroup {
                oid,
                branches: names.iter().cloned().collect(),
                keeper: keeper.clone(),
                delete_candidates: names
                    .into_iter()
                    .filter(|name| name != &keeper && !protected.contains(name))
                    .collect(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(name: &str, oid: &str) -> BranchSnapshot {
        BranchSnapshot {
            name: name.into(),
            oid: oid.into(),
            tree_oid: "same-tree".into(),
            commit_count: 1,
        }
    }

    #[test]
    fn exact_oid_groups_only_delete_unprotected_aliases() {
        let protected = BTreeSet::from([
            "main".to_owned(),
            "master".to_owned(),
            "default".to_owned(),
            "unmanaged/alias".to_owned(),
        ]);
        let groups = exact_duplicate_groups(
            [
                snapshot("main", "same-oid"),
                snapshot("master", "same-oid"),
                snapshot("default", "same-oid"),
                snapshot("unmanaged/alias", "same-oid"),
                snapshot("recovery/one", "same-oid"),
                snapshot("recovery/two", "same-oid"),
            ],
            &protected,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].keeper, "default");
        assert_eq!(
            groups[0].delete_candidates,
            ["recovery/one", "recovery/two"]
        );
    }

    #[test]
    fn same_tree_with_different_oids_is_not_duplicate() {
        let groups = exact_duplicate_groups(
            [
                snapshot("topic/short", "short-oid"),
                snapshot("topic/long", "long-oid"),
            ],
            &BTreeSet::new(),
        );
        assert!(groups.is_empty());
    }
}
