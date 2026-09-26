use serde_json::Value;
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

fn branch_only_command(remote: &Path, state: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_find-and-recovery"));
    command.args([
        "--remote",
        remote.to_str().unwrap(),
        "--state",
        state.to_str().unwrap(),
    ]);
    command.args(args);
    command.output().expect("start find-and-recovery")
}

fn branch_only_fixture(
    root: &Path,
    dirty_state: &str,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let remote = root.join("remote.git");
    let work = root.join("work");
    let state = root.join("state");
    fs::create_dir_all(root).unwrap();
    run(
        None,
        &["git", "init", "--bare", "-q", remote.to_str().unwrap()],
    );
    run(
        None,
        &["git", "init", "-q", "-b", "main", work.to_str().unwrap()],
    );
    run(Some(&work), &["git", "config", "user.name", "fixture"]);
    run(
        Some(&work),
        &["git", "config", "user.email", "fixture@example.invalid"],
    );
    fs::write(work.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(work.join("tracked.txt"), "committed content\n").unwrap();
    run(Some(&work), &["git", "add", ".gitignore", "tracked.txt"]);
    run(Some(&work), &["git", "commit", "-qm", "base"]);
    run(
        Some(&work),
        &["git", "remote", "add", "origin", remote.to_str().unwrap()],
    );
    run(
        Some(&work),
        &["git", "push", "-q", "origin", "HEAD:refs/heads/main"],
    );

    match dirty_state {
        "staged" => {
            fs::write(work.join("staged.txt"), "staged\n").unwrap();
            run(Some(&work), &["git", "add", "staged.txt"]);
        }
        "unstaged" => fs::write(work.join("tracked.txt"), "unstaged edit\n").unwrap(),
        "untracked" => fs::write(work.join("untracked.txt"), "untracked\n").unwrap(),
        "ignored" => fs::write(work.join("ignored.txt"), "ignored\n").unwrap(),
        "stash" => {
            fs::write(work.join("stash.txt"), "stashed\n").unwrap();
            run(Some(&work), &["git", "add", "stash.txt"]);
            run(
                Some(&work),
                &["git", "stash", "push", "-qm", "fixture stash"],
            );
        }
        "nested" => {
            let nested = work.join("nested");
            run(None, &["git", "init", "-q", nested.to_str().unwrap()]);
        }
        "detached" => {
            run(Some(&work), &["git", "checkout", "-q", "--detach", "HEAD"]);
            run(
                Some(&work),
                &[
                    "git",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "detached-only commit",
                ],
            );
        }
        _ => panic!("unknown dirty-state fixture: {dirty_state}"),
    }
    (remote, work, state)
}

#[test]
fn branches_only_preserve_pushes_branch_tips_and_allows_reported_omissions() {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let work = temp.path().join("work");
    let state = temp.path().join("state");
    run(
        None,
        &["git", "init", "--bare", "-q", remote.to_str().unwrap()],
    );
    run(
        None,
        &["git", "init", "-q", "-b", "main", work.to_str().unwrap()],
    );
    run(Some(&work), &["git", "config", "user.name", "fixture"]);
    run(
        Some(&work),
        &["git", "config", "user.email", "fixture@example.invalid"],
    );
    fs::write(work.join(".gitignore"), "ignored-only.txt\n").unwrap();
    fs::write(work.join("tracked.txt"), "committed content\n").unwrap();
    run(Some(&work), &["git", "add", ".gitignore", "tracked.txt"]);
    run(Some(&work), &["git", "commit", "-qm", "base"]);
    let main_oid = git(Some(&work), &["git", "rev-parse", "HEAD"]);
    run(
        Some(&work),
        &["git", "remote", "add", "origin", remote.to_str().unwrap()],
    );
    run(
        Some(&work),
        &["git", "push", "-q", "origin", "HEAD:refs/heads/main"],
    );
    run(
        Some(&work),
        &["git", "push", "-q", "origin", "HEAD:refs/heads/master"],
    );
    run(
        Some(&work),
        &["git", "push", "-q", "origin", "HEAD:refs/heads/default"],
    );
    run(Some(&work), &["git", "branch", "master"]);
    run(Some(&work), &["git", "branch", "default"]);
    run(
        None,
        &[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "symbolic-ref",
            "HEAD",
            "refs/heads/default",
        ],
    );

    // A remote-tracking-only tip and stash must not get recovery refs.
    run(
        Some(&work),
        &["git", "commit", "--allow-empty", "-qm", "extra tip"],
    );
    let extra_oid = git(Some(&work), &["git", "rev-parse", "HEAD"]);
    run(Some(&work), &["git", "reset", "--hard", "-q", &main_oid]);
    run(
        Some(&work),
        &[
            "git",
            "update-ref",
            "refs/remotes/origin/remote-only",
            &extra_oid,
        ],
    );
    fs::write(work.join("stash.txt"), "stash-only content\n").unwrap();
    run(Some(&work), &["git", "add", "stash.txt"]);
    run(Some(&work), &["git", "stash", "push", "-qm", "stash-only"]);

    // An unreferenced commit is also outside branch-only preservation scope.
    run(Some(&work), &["git", "checkout", "-qb", "temporary"]);
    run(
        Some(&work),
        &["git", "commit", "--allow-empty", "-qm", "unreachable"],
    );
    run(Some(&work), &["git", "checkout", "-q", "main"]);
    run(Some(&work), &["git", "branch", "-Dq", "temporary"]);

    // Dirty indexed and working files must not be snapshotted in this mode.
    fs::write(work.join("staged-only.txt"), "staged content\n").unwrap();
    run(Some(&work), &["git", "add", "staged-only.txt"]);
    fs::write(work.join("working-only.txt"), "working content\n").unwrap();
    fs::write(work.join("untracked-only.txt"), "untracked content\n").unwrap();
    fs::write(work.join("ignored-only.txt"), "ignored content\n").unwrap();

    let scan = Command::new(env!("CARGO_BIN_EXE_find-and-recovery"))
        .args([
            "--remote",
            remote.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "scan",
            "--roots",
            work.to_str().unwrap(),
        ])
        .output()
        .expect("start scan");
    assert!(
        scan.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );
    let preserved = Command::new(env!("CARGO_BIN_EXE_find-and-recovery"))
        .args([
            "--remote",
            remote.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "preserve",
            "--branches-only",
        ])
        .output()
        .expect("start branch-only preserve");
    assert!(
        preserved.status.success(),
        "preserve failed: {}",
        String::from_utf8_lossy(&preserved.stderr)
    );
    assert!(
        work.join("staged-only.txt").exists(),
        "preserve removed staged-only file"
    );

    let manifest: Value =
        serde_json::from_slice(&fs::read(state.join("manifest.json")).unwrap()).unwrap();
    let repo = &manifest["repositories"][0];
    let saved = repo["saved"].as_array().unwrap();
    assert_eq!(saved.len(), 3);
    for branch in ["main", "master", "default"] {
        let record = saved
            .iter()
            .find(|item| item["name"] == format!("branch:{branch}"))
            .unwrap();
        assert_eq!(record["verification"], "push-succeeded");
        assert_eq!(record["commit"], main_oid);
        assert!(
            record["remote_ref"]
                .as_str()
                .unwrap()
                .starts_with("recovery/")
        );
        let oid = git(
            None,
            &[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                &format!("refs/heads/{}", record["remote_ref"].as_str().unwrap()),
            ],
        );
        assert_eq!(oid, main_oid);
    }
    assert!(saved.iter().all(|item| item["source"] == "branch"));

    // The reserved and default remote branch tips stay unchanged.
    for branch in ["main", "master", "default"] {
        assert_eq!(
            git(
                None,
                &[
                    "git",
                    "--git-dir",
                    remote.to_str().unwrap(),
                    "rev-parse",
                    &format!("refs/heads/{branch}"),
                ],
            ),
            main_oid
        );
    }
    let refs = git(
        None,
        &[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/recovery/",
        ],
    );
    assert_eq!(refs.lines().count(), 3);

    let preview = branch_only_command(&remote, &state, &["preview", "--branches-only"]);
    assert!(preview.status.success());
    let preview_text = String::from_utf8_lossy(&preview.stdout);
    for omitted in [
        "omitted-worktree-status",
        "omitted-untracked",
        "omitted-ignored",
        "omitted-worktree-stash",
        "omitted-unreachable-commit",
    ] {
        assert!(
            preview_text.contains(omitted),
            "missing {omitted} in preview: {preview_text}"
        );
    }
    let manifest: Value =
        serde_json::from_slice(&fs::read(state.join("manifest.json")).unwrap()).unwrap();
    assert!(
        manifest["repositories"][0]["deletion"]
            .as_str()
            .unwrap()
            .starts_with("blocked-")
    );
    assert!(
        work.join("staged-only.txt").exists(),
        "preview removed staged-only file"
    );
    let cleanup = branch_only_command(
        &remote,
        &state,
        &["cleanup", "--execute", "--branches-only"],
    );
    assert!(cleanup.status.success());
    assert!(
        work.exists(),
        "branch-pushed clone with unpreserved state was deleted"
    );
    assert!(work.join("staged-only.txt").exists());
}

#[test]
fn branches_only_preserve_blocks_secondary_target_when_origin_is_different() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("velnor.git");
    let primary = temp.path().join("primary.git");
    let work = temp.path().join("work");
    let state = temp.path().join("state");
    run(
        None,
        &["git", "init", "--bare", "-q", target.to_str().unwrap()],
    );
    run(
        None,
        &["git", "init", "--bare", "-q", primary.to_str().unwrap()],
    );
    run(
        None,
        &["git", "init", "-q", "-b", "main", work.to_str().unwrap()],
    );
    run(Some(&work), &["git", "config", "user.name", "fixture"]);
    run(
        Some(&work),
        &["git", "config", "user.email", "fixture@example.invalid"],
    );
    fs::write(work.join("tracked.txt"), "committed content\n").unwrap();
    run(Some(&work), &["git", "add", "tracked.txt"]);
    run(Some(&work), &["git", "commit", "-qm", "base"]);
    run(
        Some(&work),
        &["git", "remote", "add", "origin", primary.to_str().unwrap()],
    );
    run(
        Some(&work),
        &["git", "remote", "add", "velnor", target.to_str().unwrap()],
    );

    let scan = Command::new(env!("CARGO_BIN_EXE_find-and-recovery"))
        .args([
            "--remote",
            target.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "scan",
            "--roots",
            work.to_str().unwrap(),
        ])
        .output()
        .expect("start scan");
    assert!(
        scan.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );
    let preserved = Command::new(env!("CARGO_BIN_EXE_find-and-recovery"))
        .args([
            "--remote",
            target.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "preserve",
            "--branches-only",
        ])
        .output()
        .expect("start branch-only preserve");
    assert!(
        preserved.status.success(),
        "preserve failed: {}",
        String::from_utf8_lossy(&preserved.stderr)
    );

    let manifest: Value =
        serde_json::from_slice(&fs::read(state.join("manifest.json")).unwrap()).unwrap();
    let repo = &manifest["repositories"][0];
    assert!(repo["saved"].as_array().unwrap().is_empty());
    assert!(
        repo["verification_error"]
            .as_str()
            .unwrap()
            .contains("ambiguous matching remote")
    );
    assert!(
        git(
            None,
            &[
                "git",
                "--git-dir",
                target.to_str().unwrap(),
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/recovery/",
            ]
        )
        .is_empty()
    );
}

#[test]
fn branches_only_cleanup_reports_and_blocks_omitted_work() {
    for state_kind in [
        "staged",
        "unstaged",
        "untracked",
        "ignored",
        "stash",
        "nested",
        "detached",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(state_kind);
        let (remote, work, state) = branch_only_fixture(&root, state_kind);
        for phase in [
            vec!["scan", "--roots", work.to_str().unwrap()],
            vec!["preserve", "--branches-only"],
        ] {
            let output = branch_only_command(&remote, &state, &phase);
            assert!(
                output.status.success(),
                "{} failed for {state_kind}: {}",
                phase.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let preview = branch_only_command(&remote, &state, &["preview", "--branches-only"]);
        assert!(preview.status.success());
        let preview_text = String::from_utf8_lossy(&preview.stdout);
        match state_kind {
            "staged" | "unstaged" => assert!(preview_text.contains("omitted-worktree-status")),
            "untracked" => assert!(preview_text.contains("omitted-untracked")),
            "ignored" => {
                assert!(preview_text.contains("omitted-ignored"));
                assert!(preview_text.contains("ignored.txt"));
            }
            "stash" => assert!(preview_text.contains("omitted-worktree-stash")),
            "detached" => assert!(preview_text.contains("omitted-detached-head")),
            "nested" => {}
            _ => unreachable!(),
        }
        let manifest: Value =
            serde_json::from_slice(&fs::read(state.join("manifest.json")).unwrap()).unwrap();
        let deletion = manifest["repositories"][0]["deletion"].as_str().unwrap();
        assert!(
            deletion.starts_with("blocked-"),
            "omitted {state_kind} work eligible: {deletion}"
        );
        let output = branch_only_command(
            &remote,
            &state,
            &["cleanup", "--execute", "--branches-only"],
        );
        assert!(
            output.status.success(),
            "cleanup failed for {state_kind}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            work.exists(),
            "blocked clone with omitted {state_kind} work was removed"
        );
        match state_kind {
            "staged" => assert!(work.join("staged.txt").exists()),
            "unstaged" => assert_eq!(
                fs::read_to_string(work.join("tracked.txt")).unwrap(),
                "unstaged edit\n"
            ),
            "untracked" => assert!(work.join("untracked.txt").exists()),
            "ignored" => assert!(work.join("ignored.txt").exists()),
            "stash" => assert!(!git(Some(&work), &["git", "stash", "list"]).is_empty()),
            "nested" => assert!(work.join("nested/.git").exists()),
            "detached" => assert_ne!(
                git(Some(&work), &["git", "rev-parse", "HEAD"]),
                git(Some(&work), &["git", "rev-parse", "refs/heads/main"]),
                "unpreserved detached commit was lost"
            ),
            _ => unreachable!(),
        }
    }
}

#[test]
fn branches_only_cleanup_rechecks_worktree_state_after_preview() {
    let temp = tempfile::tempdir().unwrap();
    let (remote, work, state) = branch_only_fixture(temp.path(), "untracked");
    fs::remove_file(work.join("untracked.txt")).unwrap();
    for phase in [
        vec!["scan", "--roots", work.to_str().unwrap()],
        vec!["preserve", "--branches-only"],
        vec!["preview", "--branches-only"],
    ] {
        let output = branch_only_command(&remote, &state, &phase);
        assert!(
            output.status.success(),
            "{} failed: {}",
            phase.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let manifest: Value =
        serde_json::from_slice(&fs::read(state.join("manifest.json")).unwrap()).unwrap();
    assert!(
        manifest["repositories"][0]["deletion"]
            .as_str()
            .unwrap()
            .starts_with("blocked-"),
        "branch-only push must not authorize deletion"
    );
    fs::write(work.join("after-preview.txt"), "new content\n").unwrap();

    let output = branch_only_command(
        &remote,
        &state,
        &["cleanup", "--execute", "--branches-only"],
    );
    assert!(
        output.status.success(),
        "cleanup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        work.exists(),
        "branch-only preservation cannot authorize deletion"
    );
    assert!(work.join("after-preview.txt").exists());
}
