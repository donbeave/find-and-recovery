#[path = "../src/conditional_delete.rs"]
mod conditional_delete;

use conditional_delete::{
    BranchCandidate, RefAssertion, RefState, RetentionAnchor, ReviewedDeletionFacts,
    ValidatedDestination, VerifiedDeletionScope, delete_branches_if_unchanged,
};
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn invalid_candidate_fails_before_starting_git() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let marker = temp.path().join("git-called");
    let shim = bin.join("git");
    fs::write(
        &shim,
        "#!/bin/sh\nprintf called >> \"$FNR_GIT_CALLED\"\nexit 99\n",
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();

    let old_path = std::env::var_os("PATH");
    let old_marker = std::env::var_os("FNR_GIT_CALLED");
    let mut entries = vec![bin];
    if let Some(path) = &old_path {
        entries.extend(std::env::split_paths(path));
    }
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(entries).unwrap());
        std::env::set_var("FNR_GIT_CALLED", &marker);
    }

    let oid = "1".repeat(40);
    let facts = ReviewedDeletionFacts {
        repository_identity: "example/repository".into(),
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
            retained_candidate_oids: vec![oid.clone()],
            retention_is_server_enforced: true,
        }],
        eligible_candidates: vec![BranchCandidate {
            reference: "refs/heads/candidate".into(),
            expected_oid: oid.clone(),
        }],
    };
    let scope = unsafe { VerifiedDeletionScope::attest(facts) }.unwrap();
    let destination =
        ValidatedDestination::github_https("https://github.com/example/repository.git").unwrap();
    let valid_ref = "refs/heads/candidate";
    let invalid_cases = [
        (
            vec![BranchCandidate {
                reference: "refs/tags/forbidden".into(),
                expected_oid: oid.clone(),
            }],
            "refs/heads/*",
        ),
        (
            vec![BranchCandidate {
                reference: valid_ref.into(),
                expected_oid: "not-an-object-id".into(),
            }],
            "full lowercase",
        ),
        (
            vec![
                BranchCandidate {
                    reference: valid_ref.into(),
                    expected_oid: oid.clone(),
                },
                BranchCandidate {
                    reference: valid_ref.into(),
                    expected_oid: oid.clone(),
                },
            ],
            "duplicate candidate",
        ),
        (
            vec![BranchCandidate {
                reference: valid_ref.into(),
                expected_oid: "2".repeat(40),
            }],
            "no verified durable retention anchor",
        ),
        (
            vec![BranchCandidate {
                reference: "refs/heads/main".into(),
                expected_oid: oid.clone(),
            }],
            "protected or retention ref",
        ),
        (
            vec![BranchCandidate {
                reference: "refs/heads/pull-request-head".into(),
                expected_oid: oid.clone(),
            }],
            "does not exactly match the reviewed eligible",
        ),
    ];
    for (candidates, expected_error) in invalid_cases {
        let error = delete_branches_if_unchanged(&destination, &scope, &candidates)
            .expect_err("invalid candidate set must be rejected");
        assert!(error.message.contains(expected_error), "{error}");
        assert!(!error.outcome_ambiguous);
    }

    if let Some(path) = old_path {
        unsafe { std::env::set_var("PATH", path) };
    } else {
        unsafe { std::env::remove_var("PATH") };
    }
    if let Some(value) = old_marker {
        unsafe { std::env::set_var("FNR_GIT_CALLED", value) };
    } else {
        unsafe { std::env::remove_var("FNR_GIT_CALLED") };
    }

    assert!(!marker.exists(), "invalid input invoked git");
}
