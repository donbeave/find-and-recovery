use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(cwd: Option<&Path>, args: &[&str]) -> Output {
    let mut command = Command::new(args[0]);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.args(&args[1..]).output().expect("start command");
    assert!(
        output.status.success(),
        "{} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(cwd: Option<&Path>, args: &[&str]) -> String {
    String::from_utf8_lossy(&run(cwd, args).stdout)
        .trim()
        .to_owned()
}

fn push_ref(work: &Path, remote: &Path, source: &str, destination: &str) {
    let remote = remote.to_str().unwrap();
    let refspec = format!("{source}:{destination}");
    run(Some(work), &["git", "push", "--quiet", remote, &refspec]);
}

fn remote_oid(remote: &Path, reference: &str) -> Option<String> {
    let remote = remote.to_str().unwrap();
    let output = Command::new("git")
        .args(["--git-dir", remote, "rev-parse", "--verify", reference])
        .output()
        .expect("start git");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn write_manifest(state: &Path, remote: &Path) {
    fs::create_dir_all(state).unwrap();
    let manifest = json!({
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

#[test]
fn dedupe_execute_fails_closed_without_changing_remote_refs() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = temp.path().join("work");
    let state = temp.path().join("state");

    run(
        None,
        &["git", "init", "--bare", "--quiet", remote.to_str().unwrap()],
    );
    run(
        None,
        &[
            "git",
            "init",
            "--quiet",
            "-b",
            "main",
            work.to_str().unwrap(),
        ],
    );
    run(
        Some(&work),
        &["git", "config", "user.name", "dedupe fixture"],
    );
    run(
        Some(&work),
        &["git", "config", "user.email", "dedupe@example.invalid"],
    );
    fs::write(work.join("base.txt"), "base\n").unwrap();
    run(Some(&work), &["git", "add", "base.txt"]);
    run(Some(&work), &["git", "commit", "--quiet", "-m", "base"]);
    push_ref(&work, &remote, "HEAD", "refs/heads/main");
    let base_oid = git(Some(&work), &["git", "rev-parse", "HEAD"]);
    for branch in ["master", "default", "recovery/alias"] {
        push_ref(&work, &remote, "HEAD", &format!("refs/heads/{branch}"));
    }

    run(
        Some(&work),
        &["git", "checkout", "--quiet", "-b", "history"],
    );
    fs::write(work.join("same.txt"), "same tree\n").unwrap();
    run(Some(&work), &["git", "add", "same.txt"]);
    run(
        Some(&work),
        &["git", "commit", "--quiet", "-m", "short history"],
    );
    let short_oid = git(Some(&work), &["git", "rev-parse", "HEAD"]);
    push_ref(&work, &remote, "HEAD", "refs/heads/topic/short");
    run(
        Some(&work),
        &[
            "git",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "long history",
        ],
    );
    let long_oid = git(Some(&work), &["git", "rev-parse", "HEAD"]);
    push_ref(&work, &remote, "HEAD", "refs/heads/topic/long");

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
    assert!(!output.status.success(), "dedupe --execute must fail closed");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("remote branch deletion is forbidden"),
        "unexpected error: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        remote_oid(&remote, "refs/heads/main").as_deref(),
        Some(base_oid.as_str())
    );
    for branch in ["master", "default", "recovery/alias"] {
        assert_eq!(
            remote_oid(&remote, &format!("refs/heads/{branch}")).as_deref(),
            Some(base_oid.as_str())
        );
    }
    assert_eq!(
        remote_oid(&remote, "refs/heads/topic/short").as_deref(),
        Some(short_oid.as_str())
    );
    assert_eq!(
        remote_oid(&remote, "refs/heads/topic/long").as_deref(),
        Some(long_oid.as_str())
    );
    for oid in [base_oid.as_str(), short_oid.as_str()] {
        assert_eq!(
            remote_oid(
                &remote,
                &format!("refs/archive/find-and-recovery/dedupe/{oid}")
            )
            .as_deref(),
            None,
            "failed dedupe execution created archive refs"
        );
    }
}
