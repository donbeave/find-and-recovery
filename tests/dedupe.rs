use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(cwd: Option<&Path>, args: &[&str]) -> String {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.args(args).output().expect("start git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_owned(cwd: Option<&Path>, args: Vec<String>) -> String {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.args(&args).output().expect("start git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn push_ref(work: &Path, remote: &Path, source: &str, destination: &str) {
    git_owned(
        Some(work),
        vec![
            "push".into(),
            "--quiet".into(),
            remote.display().to_string(),
            format!("{source}:{destination}"),
        ],
    );
}

fn write_manifest(state: &Path, remote: &Path) {
    fs::create_dir_all(state).unwrap();
    let manifest = serde_json::json!({
        "schema_version": 1,
        "remote": remote.display().to_string(),
        "generated_unix": 0,
        "roots": [],
        "coverage_gaps": [],
        "repositories": [],
        "deleted": []
    });
    fs::write(
        state.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn remote_branch_exists(remote: &Path, branch: &str) -> bool {
    let mut command = Command::new("git");
    let output = command
        .args([
            "--git-dir",
            remote.to_str().unwrap(),
            "show-ref",
            "--verify",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .expect("start git");
    output.status.success()
}

#[test]
fn dedupe_deletes_exact_managed_aliases_and_keeps_other_branches() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = temp.path().join("work");
    let state = temp.path().join("state");

    git(
        None,
        &["init", "--bare", "--quiet", remote.to_str().unwrap()],
    );
    git(
        None,
        &["init", "--quiet", "-b", "main", work.to_str().unwrap()],
    );
    git(Some(&work), &["config", "user.name", "dedupe fixture"]);
    git(
        Some(&work),
        &["config", "user.email", "dedupe@example.invalid"],
    );
    fs::write(work.join("base.txt"), "base\n").unwrap();
    git(Some(&work), &["add", "base.txt"]);
    git(Some(&work), &["commit", "--quiet", "-m", "base"]);
    push_ref(&work, &remote, "HEAD", "refs/heads/main");
    push_ref(&work, &remote, "HEAD", "refs/heads/master");
    push_ref(&work, &remote, "HEAD", "refs/heads/default");
    push_ref(
        &work,
        &remote,
        "HEAD",
        "refs/heads/recovery/find-and-recovery/base-alias",
    );
    git(
        None,
        &[
            "--git-dir",
            remote.to_str().unwrap(),
            "symbolic-ref",
            "HEAD",
            "refs/heads/default",
        ],
    );

    // Same committed tree and ancestry do not prove exact branch duplication.
    git(Some(&work), &["checkout", "--quiet", "-b", "history-work"]);
    fs::write(work.join("same.txt"), "same tree\n").unwrap();
    git(Some(&work), &["add", "same.txt"]);
    git(Some(&work), &["commit", "--quiet", "-m", "short history"]);
    push_ref(&work, &remote, "HEAD", "refs/heads/topic/short");
    git(
        Some(&work),
        &["commit", "--quiet", "--allow-empty", "-m", "long history"],
    );
    push_ref(&work, &remote, "HEAD", "refs/heads/topic/long");

    // These tips have identical content but unrelated commits.
    git(Some(&work), &["checkout", "--quiet", "main"]);
    git(Some(&work), &["checkout", "--quiet", "-b", "divergent-a"]);
    fs::write(work.join("divergent.txt"), "same content\n").unwrap();
    git(Some(&work), &["add", "divergent.txt"]);
    git(Some(&work), &["commit", "--quiet", "-m", "divergent A"]);
    push_ref(&work, &remote, "HEAD", "refs/heads/topic/divergent-a");
    git(Some(&work), &["checkout", "--quiet", "main"]);
    git(Some(&work), &["checkout", "--quiet", "-b", "divergent-b"]);
    fs::write(work.join("divergent.txt"), "same content\n").unwrap();
    git(Some(&work), &["add", "divergent.txt"]);
    git(Some(&work), &["commit", "--quiet", "-m", "divergent B"]);
    push_ref(&work, &remote, "HEAD", "refs/heads/topic/divergent-b");

    write_manifest(&state, &remote);
    let output = Command::new(env!("CARGO_BIN_EXE_find-and-recovery"))
        .args([
            "--remote",
            remote.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "dedupe",
            "--execute",
        ])
        .output()
        .expect("start find-and-recovery");
    assert!(
        output.status.success(),
        "dedupe failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );

    assert!(remote_branch_exists(&remote, "main"));
    assert!(remote_branch_exists(&remote, "master"));
    assert!(remote_branch_exists(&remote, "default"));
    assert!(!remote_branch_exists(
        &remote,
        "recovery/find-and-recovery/base-alias"
    ));
    assert!(remote_branch_exists(&remote, "topic/short"));
    assert!(remote_branch_exists(&remote, "topic/long"));
    assert!(remote_branch_exists(&remote, "topic/divergent-a"));
    assert!(remote_branch_exists(&remote, "topic/divergent-b"));
}
