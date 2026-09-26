//! Safe planning for duplicate remote branches.
//!
//! A branch tip can be removed only when a retained branch has the same
//! commit OID or the tip is reachable through actual commit-parent edges.
//! Tree equality, patch similarity, timestamps, and commit counts are never
//! deletion evidence.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Remote branch facts collected from one stable snapshot.
///
/// `tree_oid` and `commit_count` remain snapshot metadata for reports. The
/// planner deliberately does not use either field as proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchSnapshot {
    pub name: String,
    pub oid: String,
    pub tree_oid: String,
    pub commit_count: u64,
}

/// Relation proving that a branch tip is redundant.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DedupeRelation {
    /// The candidate and retained branch point at the exact same commit.
    ExactAlias,
    /// The candidate tip is a strict ancestor of the retained tip.
    HistoryContained,
}

impl DedupeRelation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExactAlias => "exact_alias",
            Self::HistoryContained => "history_contained",
        }
    }

    pub fn reason(self) -> &'static str {
        match self {
            Self::ExactAlias => "same commit OID",
            Self::HistoryContained => "candidate tip is an ancestor of retained tip",
        }
    }
}

/// One candidate with the exact OID and surviving branch that preserve it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlannedDeletion {
    pub branch: String,
    pub expected_oid: String,
    pub survivor_branch: String,
    pub survivor_oid: String,
    pub relation: DedupeRelation,
    pub reason: &'static str,
}

/// Global branch retention and deletion plan.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DedupePlan {
    pub retained_branches: BTreeSet<String>,
    pub deletions: Vec<PlannedDeletion>,
}

/// Compatibility shape for the exact-alias-only preview path.
///
/// New planning code must use [`plan_dedupe`] so ancestry containment is
/// handled by one global retention pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactDuplicateGroup {
    pub oid: String,
    pub branches: Vec<String>,
    pub keeper: String,
    pub delete_candidates: Vec<String>,
}

/// Group exact commit aliases for callers that only need an alias summary.
///
/// This does not prove history containment and must not be the final planner
/// for remote deletion.
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
                .find(|name| {
                    protected.contains(*name) || matches!(name.as_str(), "main" | "master")
                })
                .or_else(|| names.iter().next())?
                .clone();
            let delete_candidates = names
                .iter()
                .filter(|name| {
                    **name != keeper
                        && !protected.contains(*name)
                        && !matches!(name.as_str(), "main" | "master")
                })
                .cloned()
                .collect::<Vec<_>>();
            Some(ExactDuplicateGroup {
                oid,
                branches: names.into_iter().collect(),
                keeper,
                delete_candidates,
            })
        })
        .collect()
}

/// Plan branch deduplication against a complete commit-parent graph.
///
/// `commit_parents` maps each known commit OID to its actual parent OIDs. Root
/// commits should be present with an empty parent set. A missing row is an
/// unknown leaf: only explicit parent-edge paths can prove ancestry, and no
/// path beyond that row is inferred. The caller must obtain the graph from a
/// full, isolated, non-shallow remote snapshot with replacement refs disabled.
///
/// `eligible` is the exact set of branch names allowed into the destructive
/// plan. Callers use saved ownership for tool-created refs, or populate every
/// unprotected ref only after the operator explicitly selects that scope.
/// Every ref outside `eligible` is retained. `protected` adds active PR refs,
/// server-protected refs, configured keep-pattern matches, and other known
/// anchors that remain protected even if mistakenly listed as eligible. The
/// conventional `main` and `master` branches and advertised `default_branch`
/// are protected here as well. The default and every eligible branch must
/// exist in the same snapshot.
pub fn plan_dedupe(
    branches: impl IntoIterator<Item = BranchSnapshot>,
    commit_parents: &BTreeMap<String, BTreeSet<String>>,
    eligible: &BTreeSet<String>,
    protected: &BTreeSet<String>,
    default_branch: &str,
) -> Result<DedupePlan, String> {
    let mut by_name = BTreeMap::<String, BranchSnapshot>::new();
    for branch in branches {
        if branch.name.is_empty() || branch.oid.is_empty() {
            return Err("branch snapshot contains an empty name or OID".into());
        }
        if by_name
            .insert(branch.name.clone(), branch.clone())
            .is_some()
        {
            return Err(format!(
                "branch snapshot contains duplicate name: {}",
                branch.name
            ));
        }
    }
    if !by_name.contains_key(default_branch) {
        return Err(format!(
            "advertised default branch is absent from snapshot: {default_branch}"
        ));
    }
    for name in eligible {
        if !by_name.contains_key(name) {
            return Err(format!("eligible branch is absent from snapshot: {name}"));
        }
    }

    let mut protected = protected.clone();
    protected.insert("main".into());
    protected.insert("master".into());
    protected.insert(default_branch.to_owned());

    let mut names_by_oid = BTreeMap::<String, Vec<String>>::new();
    for branch in by_name.values() {
        names_by_oid
            .entry(branch.oid.clone())
            .or_default()
            .push(branch.name.clone());
    }
    for names in names_by_oid.values_mut() {
        names.sort();
    }

    // Walk the union of all tip histories once and build child counts. A
    // child-before-parent topological pass finds maximal tips globally,
    // including chains where every intermediate branch is also a candidate.
    let graph = reachable_parent_graph(names_by_oid.keys(), commit_parents);
    let topological = child_before_parent_order(&graph)?;

    let mut plan = DedupePlan::default();
    for branch in by_name.values() {
        if !eligible.contains(&branch.name) || protected.contains(&branch.name) {
            plan.retained_branches.insert(branch.name.clone());
        }
    }

    // `best_retained_at_or_below` carries one deterministic, durable anchor
    // from descendants toward their ancestors. Protected refs at a node are
    // retained even when they have a descendant; ordinary nodes survive only
    // when no retained descendant branch proves their history is contained.
    let mut best_retained_at_or_below = BTreeMap::<String, Survivor>::new();
    for oid in topological {
        let parents = graph.get(&oid).cloned().unwrap_or_default();
        let mut best_here = best_retained_at_or_below.get(&oid).cloned();
        let best_descendant = best_here.clone();
        if let Some(names) = names_by_oid.get(&oid) {
            let retained_names = names
                .iter()
                .filter(|name| !eligible.contains(*name) || protected.contains(*name))
                .cloned()
                .collect::<Vec<_>>();
            if !retained_names.is_empty() {
                for name in retained_names {
                    plan.retained_branches.insert(name.clone());
                    keep_better(
                        &mut best_here,
                        Some(Survivor {
                            branch: name,
                            oid: oid.clone(),
                            protected: true,
                        }),
                    );
                }
            } else if best_descendant.is_none() {
                let branch = names
                    .iter()
                    .min_by(|left, right| survivor_name_cmp(left, right))
                    .expect("a branch OID group is nonempty")
                    .clone();
                plan.retained_branches.insert(branch.clone());
                keep_better(
                    &mut best_here,
                    Some(Survivor {
                        branch,
                        oid: oid.clone(),
                        protected: false,
                    }),
                );
            }
        }

        if let Some(anchor) = best_here {
            best_retained_at_or_below.insert(oid.clone(), anchor.clone());
            for parent in parents {
                best_retained_at_or_below
                    .entry(parent)
                    .and_modify(|current| {
                        if survivor_cmp(&anchor, current).is_lt() {
                            *current = anchor.clone();
                        }
                    })
                    .or_insert_with(|| anchor.clone());
            }
        }
    }

    // Map every non-retained ref directly to a retained same-OID alias when
    // available; otherwise map it to a retained branch reachable through the
    // actual parent graph. Never route through another deletion candidate.
    for candidate in by_name.values() {
        if plan.retained_branches.contains(&candidate.name) {
            continue;
        }
        let retained_alias = names_by_oid
            .get(&candidate.oid)
            .into_iter()
            .flatten()
            .filter(|name| plan.retained_branches.contains(*name))
            .min_by(|left, right| survivor_name_cmp(left, right))
            .cloned();
        let survivor = if let Some(name) = retained_alias {
            let oid = candidate.oid.clone();
            Survivor {
                branch: name,
                oid,
                protected: protected.contains(&candidate.name),
            }
        } else {
            best_retained_at_or_below
                .get(&candidate.oid)
                .filter(|anchor| anchor.oid != candidate.oid)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "no retained branch proves recovery for candidate {} ({})",
                        candidate.name, candidate.oid
                    )
                })?
        };
        // A history-contained anchor can only reach this node through the
        // child-before-parent propagation above, so this mapping itself is the
        // proof. Avoid a second per-candidate ancestry walk here.
        let relation = if candidate.oid == survivor.oid {
            DedupeRelation::ExactAlias
        } else {
            DedupeRelation::HistoryContained
        };
        plan.deletions.push(PlannedDeletion {
            branch: candidate.name.clone(),
            expected_oid: candidate.oid.clone(),
            survivor_branch: survivor.branch,
            survivor_oid: survivor.oid,
            relation,
            reason: relation.reason(),
        });
    }

    plan.deletions
        .sort_by(|left, right| left.branch.cmp(&right.branch));
    Ok(plan)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Survivor {
    branch: String,
    oid: String,
    protected: bool,
}

fn keep_better(current: &mut Option<Survivor>, candidate: Option<Survivor>) {
    let Some(candidate) = candidate else {
        return;
    };
    if current
        .as_ref()
        .is_none_or(|incumbent| survivor_cmp(&candidate, incumbent).is_lt())
    {
        *current = Some(candidate);
    }
}

fn survivor_cmp(left: &Survivor, right: &Survivor) -> std::cmp::Ordering {
    right
        .protected
        .cmp(&left.protected)
        .then_with(|| survivor_name_cmp(&left.branch, &right.branch))
}

fn survivor_name_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    is_recovery_alias(left)
        .cmp(&is_recovery_alias(right))
        .then_with(|| left.cmp(right))
}

fn is_recovery_alias(name: &str) -> bool {
    name.starts_with("recovery/") || name.starts_with("refs/heads/recovery/")
}

fn reachable_parent_graph<'a>(
    tips: impl IntoIterator<Item = &'a String>,
    parents: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut graph = BTreeMap::<String, BTreeSet<String>>::new();
    let mut pending = tips.into_iter().cloned().collect::<VecDeque<_>>();
    while let Some(oid) = pending.pop_front() {
        if graph.contains_key(&oid) {
            continue;
        }
        let known_parents = parents.get(&oid).cloned().unwrap_or_default();
        graph.insert(oid.clone(), known_parents.clone());
        pending.extend(known_parents);
    }
    graph
}

fn child_before_parent_order(
    graph: &BTreeMap<String, BTreeSet<String>>,
) -> Result<Vec<String>, String> {
    let mut child_count = graph
        .keys()
        .map(|oid| (oid.clone(), 0usize))
        .collect::<BTreeMap<_, _>>();
    for parents in graph.values() {
        for parent in parents {
            let count = child_count
                .get_mut(parent)
                .ok_or_else(|| format!("reachable parent graph omitted commit node {parent}"))?;
            *count += 1;
        }
    }

    let mut ready = child_count
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(oid, _)| oid.clone())
        .collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(graph.len());
    while let Some(oid) = ready.pop_first() {
        order.push(oid.clone());
        if let Some(parents) = graph.get(&oid) {
            for parent in parents {
                let count = child_count
                    .get_mut(parent)
                    .expect("parent node was added to the reachable graph");
                *count -= 1;
                if *count == 0 {
                    ready.insert(parent.clone());
                }
            }
        }
    }
    if order.len() != graph.len() {
        return Err("commit-parent graph contains a cycle".into());
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(name: &str, oid: &str, tree: &str, count: u64) -> BranchSnapshot {
        BranchSnapshot {
            name: name.into(),
            oid: oid.into(),
            tree_oid: tree.into(),
            commit_count: count,
        }
    }

    fn parents(edges: &[(&str, &[&str])]) -> BTreeMap<String, BTreeSet<String>> {
        edges
            .iter()
            .map(|(oid, parents)| {
                (
                    (*oid).into(),
                    parents.iter().map(|parent| (*parent).into()).collect(),
                )
            })
            .collect()
    }

    fn plan(
        branches: Vec<BranchSnapshot>,
        graph: BTreeMap<String, BTreeSet<String>>,
        protected: &[&str],
        default_branch: &str,
    ) -> DedupePlan {
        let protected = protected
            .iter()
            .map(|name| (*name).into())
            .collect::<BTreeSet<_>>();
        let eligible = branches
            .iter()
            .filter(|branch| !protected.contains(&branch.name))
            .map(|branch| branch.name.clone())
            .collect::<BTreeSet<_>>();
        plan_dedupe(branches, &graph, &eligible, &protected, default_branch).unwrap()
    }

    #[test]
    fn exact_aliases_keep_all_protected_names_and_a_deterministic_survivor() {
        let graph = parents(&[("same", &[])]);
        let result = plan(
            vec![
                snapshot("main", "same", "tree", 1),
                snapshot("master", "same", "tree", 1),
                snapshot("default", "same", "tree", 1),
                snapshot("recovery/a", "same", "tree", 1),
                snapshot("topic/alias", "same", "tree", 1),
            ],
            graph,
            &[],
            "default",
        );
        assert_eq!(
            result.retained_branches,
            BTreeSet::from(["default".into(), "main".into(), "master".into()])
        );
        assert_eq!(result.deletions.len(), 2);
        assert!(result.deletions.iter().all(|deletion| {
            deletion.relation == DedupeRelation::ExactAlias
                && deletion.survivor_branch == "default"
                && deletion.survivor_oid == "same"
        }));
    }

    #[test]
    fn meaningful_branch_is_preferred_over_recovery_alias_for_maximal_tip() {
        let result = plan(
            vec![
                snapshot("main", "root", "tree-0", 1),
                snapshot("recovery/tip", "tip", "tree-1", 3),
                snapshot("topic/tip", "tip", "tree-1", 3),
            ],
            parents(&[("root", &[]), ("tip", &["root"])]),
            &[],
            "main",
        );
        assert_eq!(
            result.retained_branches,
            BTreeSet::from(["main".into(), "topic/tip".into()])
        );
        assert_eq!(result.deletions.len(), 1);
        assert_eq!(result.deletions[0].branch, "recovery/tip");
        assert_eq!(result.deletions[0].survivor_branch, "topic/tip");
    }

    #[test]
    fn equal_trees_and_commit_counts_do_not_prove_history() {
        let result = plan(
            vec![
                snapshot("main", "root", "same-tree", 1),
                snapshot("topic/one", "one", "same-tree", 8),
                snapshot("topic/two", "two", "same-tree", 8),
            ],
            parents(&[("root", &[]), ("one", &[]), ("two", &[])]),
            &[],
            "main",
        );
        assert!(result.deletions.is_empty());
        assert_eq!(
            result.retained_branches,
            BTreeSet::from(["main".into(), "topic/one".into(), "topic/two".into()])
        );
    }

    #[test]
    fn true_ancestor_is_removed_in_favor_of_retained_descendant() {
        let result = plan(
            vec![
                snapshot("main", "root", "tree-0", 1),
                snapshot("topic/base", "base", "tree-1", 2),
                snapshot("topic/head", "head", "tree-2", 3),
            ],
            parents(&[("root", &[]), ("base", &["root"]), ("head", &["base"])]),
            &[],
            "main",
        );
        assert_eq!(
            result.retained_branches,
            BTreeSet::from(["main".into(), "topic/head".into()])
        );
        assert_eq!(result.deletions.len(), 1);
        assert_eq!(result.deletions[0].branch, "topic/base");
        assert_eq!(result.deletions[0].survivor_branch, "topic/head");
        assert_eq!(result.deletions[0].expected_oid, "base");
        assert_eq!(
            result.deletions[0].relation,
            DedupeRelation::HistoryContained
        );
    }

    #[test]
    fn merge_tip_proves_both_parent_histories_contained() {
        let result = plan(
            vec![
                snapshot("main", "root", "tree-root", 1),
                snapshot("topic/left", "left", "tree-left", 2),
                snapshot("topic/right", "right", "tree-right", 2),
                snapshot("topic/merge", "merge", "tree-merge", 3),
            ],
            parents(&[
                ("root", &[]),
                ("left", &["root"]),
                ("right", &["root"]),
                ("merge", &["left", "right"]),
            ]),
            &[],
            "main",
        );
        assert_eq!(
            result.retained_branches,
            BTreeSet::from(["main".into(), "topic/merge".into()])
        );
        assert_eq!(result.deletions.len(), 2);
        assert!(result.deletions.iter().all(|deletion| {
            deletion.survivor_branch == "topic/merge"
                && deletion.relation == DedupeRelation::HistoryContained
        }));
    }

    #[test]
    fn protected_duplicate_refs_all_survive_and_unprotected_aliases_map_to_one() {
        let result = plan(
            vec![
                snapshot("main", "root", "tree", 1),
                snapshot("master", "root", "tree", 1),
                snapshot("topic/alias", "root", "tree", 1),
                snapshot("recovery/alias", "root", "tree", 1),
            ],
            parents(&[("root", &[])]),
            &["topic/alias"],
            "main",
        );
        assert_eq!(
            result.retained_branches,
            BTreeSet::from(["main".into(), "master".into(), "topic/alias".into()])
        );
        assert_eq!(result.deletions.len(), 1);
        assert_eq!(result.deletions[0].branch, "recovery/alias");
        assert_eq!(result.deletions[0].survivor_branch, "main");
        assert_eq!(result.deletions[0].relation, DedupeRelation::ExactAlias);
    }

    #[test]
    fn containment_chain_maps_every_candidate_directly_to_retained_tip() {
        let result = plan(
            vec![
                snapshot("main", "root", "tree-0", 1),
                snapshot("recovery/a", "a", "tree-1", 2),
                snapshot("recovery/b", "b", "tree-2", 3),
                snapshot("topic/c", "c", "tree-3", 4),
            ],
            parents(&[
                ("root", &[]),
                ("a", &["root"]),
                ("b", &["a"]),
                ("c", &["b"]),
            ]),
            &[],
            "main",
        );
        assert_eq!(
            result.retained_branches,
            BTreeSet::from(["main".into(), "topic/c".into()])
        );
        assert_eq!(result.deletions.len(), 2);
        assert!(result.deletions.iter().all(|deletion| {
            deletion.survivor_branch == "topic/c"
                && deletion.survivor_oid == "c"
                && deletion.relation == DedupeRelation::HistoryContained
        }));
    }

    #[test]
    fn missing_parent_facts_never_authorize_history_deletion() {
        let result = plan(
            vec![
                snapshot("main", "root", "tree-0", 1),
                snapshot("topic/old", "old", "same-tree", 2),
                snapshot("topic/new", "new", "same-tree", 7),
            ],
            parents(&[("root", &[])]),
            &[],
            "main",
        );
        assert!(result.deletions.is_empty());
        assert!(result.retained_branches.contains("topic/old"));
        assert!(result.retained_branches.contains("topic/new"));
    }

    #[test]
    fn explicit_parent_edge_proves_ancestry_even_if_candidate_parent_row_is_missing() {
        let result = plan(
            vec![
                snapshot("main", "root", "tree-0", 1),
                snapshot("topic/old", "old", "tree-1", 2),
                snapshot("topic/new", "new", "tree-2", 3),
            ],
            parents(&[("root", &[]), ("new", &["old"])]),
            &[],
            "main",
        );
        assert_eq!(result.deletions.len(), 1);
        assert_eq!(result.deletions[0].branch, "topic/old");
        assert_eq!(result.deletions[0].survivor_branch, "topic/new");
        assert_eq!(
            result.deletions[0].relation,
            DedupeRelation::HistoryContained
        );
    }

    #[test]
    fn missing_default_and_duplicate_names_fail_closed() {
        let empty_graph = BTreeMap::new();
        let no_protected = BTreeSet::new();
        let no_eligible = BTreeSet::new();
        let main = snapshot("main", "root", "tree", 1);
        assert!(
            plan_dedupe(
                [main.clone()],
                &empty_graph,
                &no_eligible,
                &no_protected,
                "absent"
            )
            .is_err()
        );
        assert!(
            plan_dedupe(
                [main.clone(), main],
                &empty_graph,
                &no_eligible,
                &no_protected,
                "main"
            )
            .is_err()
        );
        assert!(
            plan_dedupe(
                [snapshot("main", "root", "tree", 1)],
                &empty_graph,
                &BTreeSet::from(["absent".into()]),
                &no_protected,
                "main"
            )
            .is_err()
        );
    }

    #[test]
    fn omitted_unowned_ref_is_retained_and_can_anchor_owned_candidate() {
        let branches = vec![
            snapshot("main", "root", "tree-0", 1),
            snapshot("recovery/find-and-recovery/owned", "owned", "tree-1", 2),
            snapshot("topic/user", "user", "tree-2", 3),
        ];
        let eligible = BTreeSet::from(["recovery/find-and-recovery/owned".into()]);
        let result = plan_dedupe(
            branches,
            &parents(&[("root", &[]), ("owned", &["root"]), ("user", &["owned"])]),
            &eligible,
            &BTreeSet::new(),
            "main",
        )
        .unwrap();
        assert!(result.retained_branches.contains("topic/user"));
        assert!(
            !result
                .retained_branches
                .contains("recovery/find-and-recovery/owned")
        );
        assert_eq!(result.deletions.len(), 1);
        assert_eq!(
            result.deletions[0].branch,
            "recovery/find-and-recovery/owned"
        );
        assert_eq!(result.deletions[0].survivor_branch, "topic/user");
    }

    #[test]
    fn protected_descendant_does_not_protect_an_intermediate_candidate_as_keeper() {
        let result = plan(
            vec![
                snapshot("main", "root", "tree-0", 1),
                snapshot("recovery/base", "base", "tree-1", 2),
                snapshot("topic/head", "head", "tree-2", 3),
            ],
            parents(&[("root", &[]), ("base", &["root"]), ("head", &["base"])]),
            &["topic/head"],
            "main",
        );
        assert_eq!(
            result.retained_branches,
            BTreeSet::from(["main".into(), "topic/head".into()])
        );
        assert_eq!(result.deletions.len(), 1);
        assert_eq!(result.deletions[0].survivor_branch, "topic/head");
    }
}
