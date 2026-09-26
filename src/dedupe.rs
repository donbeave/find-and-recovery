//! Pure planning logic for exact duplicate remote recovery refs.
//!
//! This module does not inspect a remote or delete refs. Callers supply the
//! observed object IDs and manifest provenance, then decide whether to act on
//! the returned candidates.

use std::collections::{BTreeMap, BTreeSet};

/// A remote branch's name and observed commit object ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteBranch {
    pub name: String,
    pub oid: String,
}

/// Manifest ownership record for a recovery ref.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryRef {
    pub name: String,
    /// True only if this run created the remote ref. Existing identical refs
    /// must be recorded as false and are never deletion candidates.
    pub created_by_this_run: bool,
}

/// One set of branches pointing to the same exact commit OID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateGroup {
    pub oid: String,
    pub branches: Vec<String>,
    /// Owned recovery refs in this group, excluding one retained branch and
    /// every protected or preexisting ref.
    pub delete_candidates: Vec<String>,
}

/// Find exact-OID duplicate groups and safe candidate refs.
///
/// A branch is eligible for deletion only when its manifest record says this
/// run created it. Protected refs are never candidates. When a group has no
/// protected or preexisting branch, the lexicographically first branch is
/// retained so a group is never entirely removed.
pub fn exact_duplicate_groups(
    branches: impl IntoIterator<Item = RemoteBranch>,
    recovery_refs: impl IntoIterator<Item = RecoveryRef>,
    protected_refs: &BTreeSet<String>,
) -> Vec<DuplicateGroup> {
    let owned: BTreeMap<String, bool> = recovery_refs
        .into_iter()
        .map(|record| (record.name, record.created_by_this_run))
        .collect();
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
            let branches: Vec<String> = names.into_iter().collect();
            let has_safe_anchor = branches
                .iter()
                .any(|name| protected_refs.contains(name) || owned.get(name) != Some(&true));
            let mut kept_one = false;
            let delete_candidates = branches
                .iter()
                .filter(|name| owned.get(*name) == Some(&true) && !protected_refs.contains(*name))
                .filter(|_| {
                    if has_safe_anchor || kept_one {
                        true
                    } else {
                        kept_one = true;
                        false
                    }
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

    fn recovery(name: &str, created: bool) -> RecoveryRef {
        RecoveryRef {
            name: name.into(),
            created_by_this_run: created,
        }
    }

    #[test]
    fn identical_oids_form_duplicate_group_and_owned_duplicate_is_candidate() {
        let groups = exact_duplicate_groups(
            [branch("recover/a", "abc"), branch("recover/b", "abc")],
            [recovery("recover/a", true), recovery("recover/b", true)],
            &BTreeSet::new(),
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].oid, "abc");
        assert_eq!(groups[0].branches, ["recover/a", "recover/b"]);
        assert_eq!(groups[0].delete_candidates, ["recover/b"]);
    }

    #[test]
    fn different_commit_oids_are_not_duplicates_even_if_trees_might_match() {
        let groups = exact_duplicate_groups(
            [
                branch("recover/a", "commit-1"),
                branch("recover/b", "commit-2"),
            ],
            [recovery("recover/a", true), recovery("recover/b", true)],
            &BTreeSet::new(),
        );
        assert!(groups.is_empty());
    }

    #[test]
    fn protected_main_and_default_are_never_candidates() {
        let protected = ["main".to_owned(), "HEAD".to_owned()].into_iter().collect();
        let groups = exact_duplicate_groups(
            [
                branch("main", "abc"),
                branch("recover/a", "abc"),
                branch("HEAD", "def"),
                branch("recover/b", "def"),
            ],
            [recovery("recover/a", true), recovery("recover/b", true)],
            &protected,
        );
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].delete_candidates, ["recover/a"]);
        assert_eq!(groups[1].delete_candidates, ["recover/b"]);
    }

    #[test]
    fn preexisting_manifest_refs_are_not_candidates() {
        let groups = exact_duplicate_groups(
            [
                branch("recover/preexisting", "abc"),
                branch("recover/new", "abc"),
            ],
            [
                recovery("recover/preexisting", false),
                recovery("recover/new", true),
            ],
            &BTreeSet::new(),
        );
        assert_eq!(groups[0].delete_candidates, ["recover/new"]);
    }
}
