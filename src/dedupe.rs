//! Tree based planning for duplicate remote branches.

use std::collections::{BTreeMap, BTreeSet};

/// Remote branch facts collected from one stable snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchSnapshot {
    pub name: String,
    pub oid: String,
    pub tree_oid: String,
    pub commit_count: u64,
    /// Names of branch tips that contain this tip, including aliases when the
    /// two branches point at the same commit.
    pub contained_by: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeDuplicateGroup {
    pub tree_oid: String,
    pub branches: Vec<String>,
    pub keep_branch: String,
    /// Main plus one representative for each incomparable maximal history.
    pub retained_branches: Vec<String>,
    pub delete_candidates: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactDuplicateGroup {
    pub oid: String,
    pub branches: Vec<String>,
    pub keeper: String,
    pub delete_candidates: Vec<String>,
}

/// Group branches only when their exact commit IDs match. Preserve protected
/// branches (including `main`) and retain one deterministic name per group.
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

/// Group remote branches by committed tree and select one branch to retain.
///
/// Exact `main` and one representative for every incomparable maximal history
/// are retained. A branch is a deletion candidate only when its tip is an
/// ancestor of a retained tip, or when it is an exact commit alias of one.
/// This prevents commit count alone from deleting divergent history.
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
        .filter_map(|(tree_oid, names_and_counts)| {
            if names_and_counts.len() < 2 {
                return None;
            }
            let branches = names_and_counts.keys().cloned().collect::<Vec<_>>();
            let maximal_oids = names_and_counts
                .values()
                .filter(|candidate| {
                    !names_and_counts.values().any(|descendant| {
                        descendant.oid != candidate.oid
                            && candidate.contained_by.contains(&descendant.name)
                    })
                })
                .map(|branch| branch.oid.clone())
                .collect::<BTreeSet<_>>();

            // For each maximal commit, choose one name as its representative.
            // Exact OID aliases are safe to collapse because they preserve the
            // same commit object and reachable history.
            let mut retained = BTreeSet::new();
            if names_and_counts.contains_key("main") {
                retained.insert("main".to_owned());
            }
            for oid in maximal_oids {
                let representative = names_and_counts
                    .values()
                    .filter(|branch| branch.oid == oid)
                    .find(|branch| branch.name == "main")
                    .or_else(|| {
                        names_and_counts
                            .values()
                            .filter(|branch| branch.oid == oid)
                            .max_by(|left, right| {
                                left.commit_count
                                    .cmp(&right.commit_count)
                                    .then_with(|| right.name.cmp(&left.name))
                            })
                    })?;
                retained.insert(representative.name.clone());
            }
            let retained_branches = retained.into_iter().collect::<Vec<_>>();
            let keep_branch = retained_branches
                .iter()
                .find(|name| name.as_str() == "main")
                .cloned()
                .or_else(|| {
                    retained_branches
                        .iter()
                        .max_by(|left, right| {
                            names_and_counts[*left]
                                .commit_count
                                .cmp(&names_and_counts[*right].commit_count)
                                .then_with(|| right.cmp(left))
                        })
                        .cloned()
                })?;
            let delete_candidates = branches
                .iter()
                .filter(|name| {
                    if retained_branches.contains(name) {
                        return false;
                    }
                    let candidate = &names_and_counts[*name];
                    retained_branches.iter().any(|retained_name| {
                        let retained_branch = &names_and_counts[retained_name];
                        candidate.oid == retained_branch.oid
                            || candidate.contained_by.contains(retained_name)
                    })
                })
                .cloned()
                .collect();
            Some(TreeDuplicateGroup {
                tree_oid,
                branches,
                keep_branch,
                retained_branches,
                delete_candidates,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(name: &str, oid: &str, tree_oid: &str, commit_count: u64) -> BranchSnapshot {
        BranchSnapshot {
            name: name.into(),
            oid: oid.into(),
            tree_oid: tree_oid.into(),
            commit_count,
            contained_by: BTreeSet::new(),
        }
    }

    #[test]
    fn exact_duplicates_only_and_protected_name_wins() {
        let protected = BTreeSet::from(["main".to_owned(), "stable".to_owned()]);
        let groups = exact_duplicate_groups(
            [
                snapshot("alias", "same-oid", "tree", 3),
                snapshot("main", "same-oid", "tree", 3),
                snapshot("stable", "same-oid", "tree", 3),
                snapshot("same-tree-different-tip", "other-oid", "tree", 4),
            ],
            &protected,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].keeper, "main");
        assert_eq!(groups[0].delete_candidates, ["alias"]);
    }

    fn contained_snapshot(
        name: &str,
        oid: &str,
        tree_oid: &str,
        commit_count: u64,
        contained_by: &[&str],
    ) -> BranchSnapshot {
        BranchSnapshot {
            contained_by: contained_by.iter().map(|name| (*name).into()).collect(),
            ..snapshot(name, oid, tree_oid, commit_count)
        }
    }

    #[test]
    fn same_tree_different_commit_ids_are_duplicates() {
        let groups = tree_duplicate_groups([
            contained_snapshot(
                "recovery/older",
                "commit-a",
                "same-tree",
                4,
                &["recovery/newer"],
            ),
            snapshot("recovery/newer", "commit-b", "same-tree", 7),
        ]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].keep_branch, "recovery/newer");
        assert_eq!(groups[0].delete_candidates, ["recovery/older"]);
    }

    #[test]
    fn most_commits_wins_and_ties_use_lexical_order() {
        let groups = tree_duplicate_groups([
            snapshot("zeta", "z", "tree", 5),
            snapshot("alpha", "a", "tree", 5),
            contained_snapshot("middle", "m", "tree", 2, &["alpha"]),
        ]);
        assert_eq!(groups[0].keep_branch, "alpha");
        assert_eq!(groups[0].retained_branches, ["alpha", "zeta"]);
        assert_eq!(groups[0].delete_candidates, ["middle"]);
    }

    #[test]
    fn main_is_kept_even_when_its_history_is_shorter() {
        let groups = tree_duplicate_groups([
            contained_snapshot("main", "main-tip", "same-tree", 1, &["recovery/long"]),
            snapshot("recovery/long", "long-tip", "same-tree", 40),
        ]);
        assert_eq!(groups[0].keep_branch, "main");
        assert!(
            groups[0]
                .retained_branches
                .contains(&"recovery/long".to_owned())
        );
        assert!(groups[0].delete_candidates.is_empty());
    }

    #[test]
    fn ordinary_remote_branch_can_be_deleted() {
        let groups = tree_duplicate_groups([
            contained_snapshot("release", "r", "same-tree", 1, &["team/topic"]),
            snapshot("team/topic", "u", "same-tree", 10),
        ]);
        assert_eq!(groups[0].keep_branch, "team/topic");
        assert_eq!(groups[0].delete_candidates, ["release"]);
    }

    #[test]
    fn branch_named_master_is_not_implicitly_protected() {
        let groups = tree_duplicate_groups([
            contained_snapshot("master", "m", "same-tree", 1, &["topic"]),
            snapshot("topic", "t", "same-tree", 2),
        ]);
        assert_eq!(groups[0].keep_branch, "topic");
        assert_eq!(groups[0].delete_candidates, ["master"]);
    }

    #[test]
    fn different_trees_are_not_duplicates() {
        assert!(
            tree_duplicate_groups([
                snapshot("one", "c1", "tree-one", 2),
                snapshot("two", "c2", "tree-two", 3),
            ])
            .is_empty()
        );
    }

    #[test]
    fn same_tree_diverged_tips_are_both_retained() {
        let groups = tree_duplicate_groups([
            snapshot("longer", "long", "same-tree", 30),
            snapshot("shorter", "short", "same-tree", 10),
        ]);
        assert_eq!(groups[0].keep_branch, "longer");
        assert_eq!(groups[0].retained_branches, ["longer", "shorter"]);
        assert!(groups[0].delete_candidates.is_empty());
    }

    #[test]
    fn ancestor_tip_is_deleted_when_longer_tip_is_retained() {
        let groups = tree_duplicate_groups([
            contained_snapshot("short", "s", "same-tree", 5, &["long"]),
            snapshot("long", "l", "same-tree", 12),
        ]);
        assert_eq!(groups[0].keep_branch, "long");
        assert_eq!(groups[0].retained_branches, ["long"]);
        assert_eq!(groups[0].delete_candidates, ["short"]);
    }

    #[test]
    fn exact_commit_alias_can_be_deleted() {
        let groups = tree_duplicate_groups([
            snapshot("aaa-alias", "same-oid", "same-tree", 8),
            snapshot("main", "same-oid", "same-tree", 8),
            snapshot("topic", "same-oid", "same-tree", 8),
        ]);
        assert_eq!(groups[0].retained_branches, ["main"]);
        assert_eq!(groups[0].delete_candidates, ["aaa-alias", "topic"]);
    }

    #[test]
    fn exact_planner_ignores_same_tree_branches_with_different_oids() {
        let groups = exact_duplicate_groups(
            [
                snapshot("topic/short", "short-oid", "same-tree", 2),
                snapshot("topic/long", "long-oid", "same-tree", 3),
            ],
            &BTreeSet::from(["main".to_owned()]),
        );
        assert!(groups.is_empty());
    }

    #[test]
    fn exact_planner_never_marks_main_as_candidate() {
        let groups = exact_duplicate_groups(
            [
                snapshot("main", "shared-oid", "tree", 4),
                snapshot("recovery/one", "shared-oid", "tree", 4),
                snapshot("recovery/two", "shared-oid", "tree", 4),
            ],
            &BTreeSet::from(["main".to_owned()]),
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].keeper, "main");
        assert_eq!(
            groups[0].delete_candidates,
            ["recovery/one", "recovery/two"]
        );
        assert!(
            !groups[0]
                .delete_candidates
                .iter()
                .any(|name| name == "main")
        );
    }

    #[test]
    fn exact_planner_keeps_default_and_master_even_when_they_are_aliases() {
        let protected =
            BTreeSet::from(["main".to_owned(), "master".to_owned(), "default".to_owned()]);
        let groups = exact_duplicate_groups(
            [
                snapshot("main", "shared-oid", "tree", 4),
                snapshot("master", "shared-oid", "tree", 4),
                snapshot("default", "shared-oid", "tree", 4),
                snapshot("recovery/find-and-recovery/alias", "shared-oid", "tree", 4),
            ],
            &protected,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].keeper, "default");
        assert_eq!(
            groups[0].delete_candidates,
            ["recovery/find-and-recovery/alias"]
        );
    }
}
