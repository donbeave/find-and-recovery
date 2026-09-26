use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn git(cwd: Option<&Path>, args: &[&str]) -> Output {
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
    output
}

fn git_text(cwd: Option<&Path>, args: &[&str]) -> String {
    String::from_utf8_lossy(&git(cwd, args).stdout)
        .trim()
        .to_owned()
}

fn cli(root: &Path, remote: &Path, state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_find-and-recovery"))
        .args(["--remote", remote.to_str().unwrap()])
        .args(["--state", state.to_str().unwrap()])
        .args(args)
        .env("TMPDIR", root)
        .output()
        .expect("start find-and-recovery")
}

fn assert_cli_ok(output: &Output, phase: &str) {
    assert!(
        output.status.success(),
        "{phase} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn deleting_manifest_worktree_snapshot_row_cannot_authorize_dirty_clone_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let remote = root.join("target.git");
    let clone = root.join("copies/working clone");
    let state = root.join("state");
    fs::create_dir_all(clone.parent().unwrap()).unwrap();

    git(
        None,
        &[
            "init",
            "--bare",
            "--quiet",
            "--initial-branch=main",
            remote.to_str().unwrap(),
        ],
    );
    git(
        None,
        &[
            "init",
            "--quiet",
            "--initial-branch=main",
            clone.to_str().unwrap(),
        ],
    );
    git(Some(&clone), &["config", "user.name", "fixture"]);
    git(
        Some(&clone),
        &["config", "user.email", "fixture@example.invalid"],
    );
    fs::write(clone.join("tracked.txt"), "committed\n").unwrap();
    git(Some(&clone), &["add", "tracked.txt"]);
    git(Some(&clone), &["commit", "--quiet", "-m", "base"]);
    git(
        Some(&clone),
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(Some(&clone), &["push", "--quiet", "-u", "origin", "main"]);

    // Keep two distinct dirty states: a modified tracked file and an untracked file.
    fs::write(clone.join("tracked.txt"), "uncommitted private state\n").unwrap();
    fs::write(clone.join("untracked.txt"), "uncommitted second file\n").unwrap();

    let scan = cli(
        root,
        &remote,
        &state,
        &["scan", "--roots", clone.parent().unwrap().to_str().unwrap()],
    );
    assert_cli_ok(&scan, "scan");
    let preserve = cli(root, &remote, &state, &["preserve"]);
    assert_cli_ok(&preserve, "preserve");
    let verify = cli(root, &remote, &state, &["verify"]);
    assert_cli_ok(&verify, "verify");

    let manifest_path = state.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let saved = manifest["repositories"][0]["saved"].as_array_mut().unwrap();
    assert!(saved.iter().any(|row| row["source"] == "worktree-snapshot"));
    saved.retain(|row| row["source"] != "worktree-snapshot");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let cleanup = cli(root, &remote, &state, &["cleanup", "--execute"]);
    assert!(
        clone.is_dir(),
        "cleanup deleted dirty local state after its snapshot proof row was removed; stdout={} stderr={}",
        String::from_utf8_lossy(&cleanup.stdout),
        String::from_utf8_lossy(&cleanup.stderr)
    );
    let after: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert!(
        after["repositories"][0]["deletion"]
            .as_str()
            .unwrap()
            .starts_with("blocked"),
        "cleanup did not record a blocked result: {}; stdout={}",
        after["repositories"][0]["deletion"],
        String::from_utf8_lossy(&cleanup.stdout)
    );
    assert_eq!(
        fs::read_to_string(clone.join("tracked.txt")).unwrap(),
        "uncommitted private state\n"
    );
    assert_eq!(
        fs::read_to_string(clone.join("untracked.txt")).unwrap(),
        "uncommitted second file\n"
    );
    assert_eq!(
        git_text(
            None,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main"
            ]
        ),
        git_text(Some(&clone), &["rev-parse", "refs/heads/main"]),
        "the source default branch remains untouched"
    );
}
