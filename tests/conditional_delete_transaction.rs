#[path = "../src/conditional_delete.rs"]
mod conditional_delete;

use conditional_delete::{Candidate, delete_candidates_if_unchanged};
use std::process::Command;

fn git(args: &[&str]) -> String {
    let output = Command::new("git").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn transaction_entrypoint_refuses_without_changing_any_remote_ref() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = temp.path().join("work");
    git(&["init", "--bare", "-q", remote.to_str().unwrap()]);
    git(&["init", "-q", work.to_str().unwrap()]);
    git(&[
        "-C",
        work.to_str().unwrap(),
        "config",
        "user.name",
        "fixture",
    ]);
    git(&[
        "-C",
        work.to_str().unwrap(),
        "config",
        "user.email",
        "fixture@example.invalid",
    ]);
    std::fs::write(work.join("file"), "retained\n").unwrap();
    git(&["-C", work.to_str().unwrap(), "add", "file"]);
    git(&[
        "-C",
        work.to_str().unwrap(),
        "commit",
        "-q",
        "-m",
        "fixture",
    ]);
    git(&[
        "-C",
        work.to_str().unwrap(),
        "push",
        "-q",
        remote.to_str().unwrap(),
        "HEAD:refs/heads/main",
        "HEAD:refs/heads/candidate",
    ]);
    let before = git(&["ls-remote", "--refs", remote.to_str().unwrap()]);
    let oid = git(&["-C", work.to_str().unwrap(), "rev-parse", "HEAD"]);

    let error = delete_candidates_if_unchanged(
        remote.to_str().unwrap(),
        &[Candidate {
            remote_ref: "refs/heads/candidate".into(),
            expected_oid: oid.clone(),
            keeper_ref: "refs/heads/main".into(),
            keeper_oid: oid,
        }],
    )
    .unwrap_err();

    assert!(error.contains("disabled by policy"), "{error}");
    let after = git(&["ls-remote", "--refs", remote.to_str().unwrap()]);
    assert_eq!(after, before);
}
