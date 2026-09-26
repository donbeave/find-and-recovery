#[path = "../src/conditional_delete.rs"]
mod conditional_delete;

use conditional_delete::{
    BranchCandidate, Candidate, RefAssertion, RefState, RetentionAnchor, ReviewedDeletionFacts,
    ValidatedDestination, VerifiedDeletionScope, delete_candidates_if_unchanged,
};

fn reviewed_facts(identity: &str) -> ReviewedDeletionFacts {
    let oid = "a".repeat(40);
    ReviewedDeletionFacts {
        repository_identity: identity.into(),
        actual_default_branch: RefAssertion {
            reference: "refs/heads/main".into(),
            expected: RefState::Present(oid.clone()),
        },
        main_branch: RefAssertion {
            reference: "refs/heads/main".into(),
            expected: RefState::Present(oid.clone()),
        },
        master_branch: RefAssertion {
            reference: "refs/heads/master".into(),
            expected: RefState::Absent,
        },
        protections_and_rulesets_reviewed: true,
        server_rejects_active_default_branch_deletion: true,
        pull_request_heads_and_bases_reviewed: true,
        keep_patterns_reviewed: true,
        retention_anchors: vec![RetentionAnchor {
            reference: "refs/heads/retention-anchor".into(),
            expected_oid: oid.clone(),
            retained_candidate_oids: vec![oid],
            retention_is_server_enforced: true,
        }],
        eligible_candidates: vec![],
    }
}

#[test]
fn legacy_delete_entrypoint_fails_closed_without_verified_scope() {
    let oid = "b".repeat(40);
    let candidate = Candidate {
        remote_ref: "refs/heads/candidate".into(),
        expected_oid: oid.clone(),
        keeper_ref: "refs/heads/main".into(),
        keeper_oid: oid,
    };

    let error =
        delete_candidates_if_unchanged("https://github.com/example/repository.git", &[candidate])
            .unwrap_err();
    assert!(error.contains("disabled by policy"));
    assert!(error.contains("verified safety and retention scope"));
}

#[test]
fn github_destination_parser_rejects_non_github_or_ambiguous_urls() {
    let destination =
        ValidatedDestination::github_https("https://github.com/example/repository.git").unwrap();
    let mismatched =
        unsafe { VerifiedDeletionScope::attest(reviewed_facts("other/repository")) }.unwrap();

    let error = conditional_delete::delete_branches_if_unchanged(&destination, &mismatched, &[])
        .unwrap_err();
    assert!(error.message.contains("does not match"));
    assert!(!error.outcome_ambiguous);

    assert!(
        ValidatedDestination::github_https("https://github.com.evil/example/repository").is_err()
    );
    assert!(
        ValidatedDestination::github_https("https://github.com/example/repository?mirror=other")
            .is_err()
    );
    assert!(
        ValidatedDestination::github_https("ssh://git@github.com/example/repository.git").is_err()
    );
    assert!(
        ValidatedDestination::github_https("https://user@github.com/example/repository.git")
            .is_err()
    );
}

#[test]
fn deletion_scope_requires_all_external_reviews_and_an_anchor() {
    let mut facts = reviewed_facts("example/repository");
    facts.keep_patterns_reviewed = false;
    let error = unsafe { VerifiedDeletionScope::attest(facts) }.unwrap_err();
    assert!(error.contains("keep patterns"));

    let mut facts = reviewed_facts("example/repository");
    facts.retention_anchors.clear();
    let error = unsafe { VerifiedDeletionScope::attest(facts) }.unwrap_err();
    assert!(error.contains("retention anchor"));
}

#[test]
fn candidate_requests_are_bound_to_the_reviewed_refs_and_oids() {
    let oid = "a".repeat(40);
    let mut facts = reviewed_facts("example/repository");
    facts.eligible_candidates = vec![BranchCandidate {
        reference: "refs/heads/reviewed-safe-branch".into(),
        expected_oid: oid.clone(),
    }];
    let scope = unsafe { VerifiedDeletionScope::attest(facts) }.unwrap();
    let destination =
        ValidatedDestination::github_https("https://github.com/example/repository.git").unwrap();

    let error = conditional_delete::delete_branches_if_unchanged(
        &destination,
        &scope,
        &[BranchCandidate {
            reference: "refs/heads/pull-request-head".into(),
            expected_oid: oid.clone(),
        }],
    )
    .unwrap_err();
    assert!(error.message.contains("does not exactly match"));
    assert!(!error.outcome_ambiguous);

    let mut facts = reviewed_facts("example/repository");
    facts.eligible_candidates = vec![BranchCandidate {
        reference: "refs/heads/main".into(),
        expected_oid: oid,
    }];
    let error = unsafe { VerifiedDeletionScope::attest(facts) }.unwrap_err();
    assert!(error.contains("protected or retention ref"));
}
