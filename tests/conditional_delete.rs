#[path = "../src/conditional_delete.rs"]
mod conditional_delete;

use conditional_delete::{delete_candidates_if_unchanged, delete_if_unchanged, Candidate};
use std::path::Path;
use std::process::Command;

fn git(cwd: Option<&Path>, args: &[&str]) -> String {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.arg("-C").arg(cwd);
    }
    let output = command.args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn all_remote_delete_entrypoints_refuse_without_changing_refs() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = temp.path().join("work");
    git(None, &["init", "--bare", "-q", remote.to_str().unwrap()]);
    git(None, &["init", "-q", work.to_str().unwrap()]);
    git(Some(&work), &["config", "user.name", "policy test"]);
    git(
        Some(&work),
        &["config", "user.email", "policy@example.invalid"],
    );
    std::fs::write(work.join("file"), "kept\n").unwrap();
    git(Some(&work), &["add", "file"]);
    git(Some(&work), &["commit", "-q", "-m", "fixture"]);
    git(
        Some(&work),
        &[
            "push",
            "-q",
            remote.to_str().unwrap(),
            "HEAD:refs/heads/main",
            "HEAD:refs/heads/candidate",
        ],
    );
    let before = git(None, &["ls-remote", "--refs", remote.to_str().unwrap()]);
    let oid = git(Some(&work), &["rev-parse", "HEAD"]);
    let candidate = Candidate {
        remote_ref: "refs/heads/candidate".into(),
        expected_oid: oid.clone(),
        keeper_ref: "refs/heads/main".into(),
        keeper_oid: oid.clone(),
    };

    let error = delete_candidates_if_unchanged(remote.to_str().unwrap(), &[candidate]).unwrap_err();
    assert!(error.contains("disabled by policy"));

    #[allow(deprecated)]
    let error = delete_if_unchanged(
        remote.to_str().unwrap(),
        "refs/heads/candidate",
        &oid,
        "refs/heads/main",
        &oid,
    )
    .unwrap_err();
    assert!(error.contains("disabled by policy"));

    let after = git(None, &["ls-remote", "--refs", remote.to_str().unwrap()]);
    assert_eq!(after, before, "policy fence changed remote refs");
}
