#[path = "../src/conditional_delete.rs"]
mod conditional_delete;

use conditional_delete::{Candidate, delete_candidates_if_unchanged};
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn both_delete_apis_fail_closed_without_invoking_git() {
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

    let candidate = Candidate {
        remote_ref: "refs/heads/topic-to-remove".into(),
        expected_oid: "1".repeat(40),
        keeper_ref: "refs/heads/retained-topic".into(),
        keeper_oid: "2".repeat(40),
    };
    let remote = "https://example.invalid/owner/repository.git";
    let current = delete_candidates_if_unchanged(remote, &[candidate.clone()]);
    #[allow(deprecated)]
    let legacy = conditional_delete::delete_if_unchanged(
        remote,
        &candidate.remote_ref,
        &candidate.expected_oid,
        &candidate.keeper_ref,
        &candidate.keeper_oid,
    );

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

    for result in [current, legacy] {
        let error = result.expect_err("remote branch deletion must be disabled by policy");
        assert!(
            error.contains("disabled by policy"),
            "unexpected error: {error}"
        );
    }
    assert!(!marker.exists(), "disabled APIs invoked git");
}
