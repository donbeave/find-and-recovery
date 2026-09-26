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

/// Plan cleanup for branches with identical committed root trees.
///
/// Keep the branch with the highest reachable commit count (lexicographically
/// smallest name breaks ties) and always keep exact `main`. Every other branch
/// is a deletion candidate only after its tip has been archived under the
/// corresponding create-only ref. Branches sharing an OID share one archive
/// ref because the ref preserves the same commit object and history.
pub fn tree_duplicate_groups(
    branches: impl IntoIterator<Item = BranchSnapshot>,
) -> Vec<TreeDuplicateGroup> {
    let mut by_tree = BTreeMap::<String, BTreeMap<String, BranchSnapshot>>::new();
    for branch in branches {
        by_tree
            .entry(branch.tree_oid.clone())
            .or_default()
            .entry(branch.name.clone())
            .or_insert(branch);
    }

    by_tree
        .into_iter()
        .filter_map(|(tree_oid, snapshots)| {
            if snapshots.len() < 2 {
                return None;
            }
            let branches = snapshots.keys().cloned().collect::<Vec<_>>();
            let winner = snapshots.values().max_by(|left, right| {
                left.commit_count
                    .cmp(&right.commit_count)
                    .then_with(|| right.name.cmp(&left.name))
            })?;
            let keep_branch = winner.name.clone();
            let mut retained = BTreeSet::from([keep_branch.clone()]);
            if snapshots.contains_key("main") {
                retained.insert("main".to_owned());
            }
            let retained_branches = retained.into_iter().collect::<Vec<_>>();
            let delete_candidates = branches
                .iter()
                .filter(|name| !retained_branches.contains(name))
                .cloned()
                .collect::<Vec<_>>();
            let archive_refs = delete_candidates
                .iter()
                .map(|name| {
                    let oid = &snapshots[name].oid;
                    (
                        name.clone(),
                        format!("refs/archive/find-and-recovery/dedupe/{oid}"),
                    )
                })
                .collect();
            Some(TreeDuplicateGroup {
                tree_oid,
                branches,
                keep_branch,
                retained_branches,
                delete_candidates,
                archive_refs,
            })
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeDuplicateGroup {
    pub tree_oid: String,
    pub branches: Vec<String>,
    /// Highest commit-count branch (deterministic lexical tie break).
    pub keep_branch: String,
    /// Winner plus exact `main` when they differ.
    pub retained_branches: Vec<String>,
    pub delete_candidates: Vec<String>,
    /// Candidate branch to create-only archive ref mapping.
    pub archive_refs: BTreeMap<String, String>,
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

    fn tree_snapshot(name: &str, oid: &str, tree_oid: &str, commit_count: u64) -> BranchSnapshot {
        BranchSnapshot {
            name: name.into(),
            oid: oid.into(),
            tree_oid: tree_oid.into(),
            commit_count,
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

    #[test]
    fn tree_groups_keep_greatest_commit_count() {
        let groups = tree_duplicate_groups([
            tree_snapshot("topic/short", "short", "tree", 3),
            tree_snapshot("topic/long", "long", "tree", 8),
        ]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].keep_branch, "topic/long");
        assert_eq!(groups[0].retained_branches, ["topic/long"]);
        assert_eq!(groups[0].delete_candidates, ["topic/short"]);
        assert_eq!(
            groups[0].archive_refs["topic/short"],
            "refs/archive/find-and-recovery/dedupe/short"
        );
    }

    #[test]
    fn tree_ties_choose_lexicographically_smallest_branch() {
        let groups = tree_duplicate_groups([
            tree_snapshot("zeta", "z", "tree", 5),
            tree_snapshot("alpha", "a", "tree", 5),
            tree_snapshot("middle", "m", "tree", 2),
        ]);
        assert_eq!(groups[0].keep_branch, "alpha");
        assert_eq!(groups[0].delete_candidates, ["middle", "zeta"]);
    }

    #[test]
    fn main_is_retained_with_count_winner() {
        let groups = tree_duplicate_groups([
            tree_snapshot("main", "main-tip", "tree", 1),
            tree_snapshot("topic/winner", "winner", "tree", 10),
            tree_snapshot("topic/other", "other", "tree", 4),
        ]);
        assert_eq!(groups[0].keep_branch, "topic/winner");
        assert_eq!(groups[0].retained_branches, ["main", "topic/winner"]);
        assert_eq!(groups[0].delete_candidates, ["topic/other"]);
    }

    #[test]
    fn ordinary_and_master_branches_can_be_candidates() {
        let groups = tree_duplicate_groups([
            tree_snapshot("master", "m", "tree", 1),
            tree_snapshot("team/topic", "t", "tree", 3),
            tree_snapshot("recovery/old", "r", "tree", 2),
        ]);
        assert_eq!(groups[0].keep_branch, "team/topic");
        assert_eq!(groups[0].delete_candidates, ["master", "recovery/old"]);
    }

    #[test]
    fn aliases_share_one_create_only_archive_ref() {
        let groups = tree_duplicate_groups([
            tree_snapshot("main", "same", "tree", 4),
            tree_snapshot("topic/alias-a", "same", "tree", 4),
            tree_snapshot("topic/alias-b", "same", "tree", 4),
        ]);
        assert_eq!(groups[0].retained_branches, ["main"]);
        assert_eq!(
            groups[0].delete_candidates,
            ["topic/alias-a", "topic/alias-b"]
        );
        assert_eq!(
            groups[0].archive_refs["topic/alias-a"],
            groups[0].archive_refs["topic/alias-b"]
        );
        assert_eq!(
            groups[0].archive_refs["topic/alias-b"],
            "refs/archive/find-and-recovery/dedupe/same"
        );
    }

    #[test]
    fn different_root_trees_are_not_grouped() {
        assert!(
            tree_duplicate_groups([
                tree_snapshot("one", "a", "tree-one", 2),
                tree_snapshot("two", "b", "tree-two", 9),
            ])
            .is_empty()
        );
    }
}
