use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
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

fn remote_refs(remote: &Path) -> BTreeMap<String, String> {
    let output = Command::new("git")
        .args([
            "--git-dir",
            remote.to_str().unwrap(),
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
        ])
        .output()
        .expect("start git");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("ref listing is UTF-8")
        .lines()
        .map(|line| {
            let (name, oid) = line.split_once('\0').expect("formatted ref entry");
            (name.to_owned(), oid.to_owned())
        })
        .collect()
}

fn write_manifest(state: &Path, remote: &Path) {
    write_manifest_owned(state, remote, &[]);
}

fn write_manifest_owned(state: &Path, remote: &Path, owned: &[(&str, &str)]) {
    fs::create_dir_all(state).unwrap();
    let recovery_ownership = owned
        .iter()
        .map(|(name, commit)| {
            json!({
                "source": "branch",
                "name": format!("branch:{name}"),
                "commit": commit,
                "remote_ref": name,
                "created_by_this_run": false,
                "retained_ref": null,
                "tree": null,
                "verification": "isolated-verified"
            })
        })
        .collect::<Vec<_>>();
    let manifest = json!({
        "schema_version": 1,
        "remote": remote.display().to_string(),
        "generated_unix": 0,
        "roots": [],
        "coverage_gaps": [],
        "repositories": [],
        "recovery_ownership": recovery_ownership,
        "deleted": []
    });
    fs::write(
        state.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn init_preview_fixture(parent: &Path) -> (PathBuf, PathBuf, PathBuf, String) {
    let remote = parent.join("remote.git");
    let work = parent.join("work");
    let state = parent.join("state");
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
    let base_oid = git(Some(&work), &["git", "rev-parse", "HEAD"]);
    push_ref(&work, &remote, "HEAD", "refs/heads/main");
    run(
        None,
        &[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ],
    );
    (remote, work, state, base_oid)
}

#[test]
fn execute_never_mutates_unowned_or_protected_branches() {
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

    let refs_before = remote_refs(&remote);
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
    // No branch in this fixture is owned by the manifest, and main/master
    // are mandatory protected refs. Execution may report a no-op or reject
    // the missing explicit scope, but it must never mutate any advertised ref.
    let _execution_result = output;
    assert_eq!(remote_refs(&remote), refs_before);

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

#[test]
fn preview_keeps_incomparable_histories_with_identical_trees() {
    let temp = tempfile::tempdir().unwrap();
    let (remote, work, state, base_oid) = init_preview_fixture(temp.path());
    let branch_a = "recovery/find-and-recovery/test/a";
    let branch_b = "recovery/find-and-recovery/test/b";

    run(
        Some(&work),
        &["git", "checkout", "--quiet", "-b", branch_a, "main"],
    );
    fs::write(work.join("same.txt"), "same tree\n").unwrap();
    run(Some(&work), &["git", "add", "same.txt"]);
    run(Some(&work), &["git", "commit", "--quiet", "-m", "branch a"]);
    let oid_a = git(Some(&work), &["git", "rev-parse", "HEAD"]);
    let tree_a = git(Some(&work), &["git", "rev-parse", "HEAD^{tree}"]);
    push_ref(&work, &remote, "HEAD", &format!("refs/heads/{branch_a}"));

    run(
        Some(&work),
        &["git", "checkout", "--quiet", "-b", branch_b, "main"],
    );
    fs::write(work.join("same.txt"), "same tree\n").unwrap();
    run(Some(&work), &["git", "add", "same.txt"]);
    run(Some(&work), &["git", "commit", "--quiet", "-m", "branch b"]);
    let oid_b = git(Some(&work), &["git", "rev-parse", "HEAD"]);
    let tree_b = git(Some(&work), &["git", "rev-parse", "HEAD^{tree}"]);
    push_ref(&work, &remote, "HEAD", &format!("refs/heads/{branch_b}"));

    assert_ne!(oid_a, oid_b);
    assert_eq!(tree_a, tree_b);
    write_manifest_owned(&state, &remote, &[(branch_a, &oid_a), (branch_b, &oid_b)]);
    let output = Command::new(env!("CARGO_BIN_EXE_find-and-recovery"))
        .args([
            "--remote",
            remote.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "dedupe",
        ])
        .output()
        .expect("start find-and-recovery");
    assert!(
        output.status.success(),
        "dedupe preview failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let preview = String::from_utf8_lossy(&output.stdout);
    assert!(
        !preview.contains("preview-delete") && !preview.contains("history_contained"),
        "identical trees do not prove branch containment: {preview}"
    );
    assert_eq!(
        remote_oid(&remote, "refs/heads/main").as_deref(),
        Some(base_oid.as_str())
    );
    assert_eq!(
        remote_oid(&remote, &format!("refs/heads/{branch_a}")).as_deref(),
        Some(oid_a.as_str())
    );
    assert_eq!(
        remote_oid(&remote, &format!("refs/heads/{branch_b}")).as_deref(),
        Some(oid_b.as_str())
    );
}
