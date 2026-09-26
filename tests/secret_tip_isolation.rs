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
        "{phase} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn manifest(state: &Path) -> Value {
    serde_json::from_slice(&fs::read(state.join("manifest.json")).unwrap()).unwrap()
}

#[test]
fn unsafe_tip_blocks_itself_but_safe_branch_is_still_pushed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let remote = root.join("target.git");
    let repo = root.join("clone");
    let state = root.join("state");

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
            repo.to_str().unwrap(),
        ],
    );
    git(Some(&repo), &["config", "user.name", "Recovery fixture"]);
    git(
        Some(&repo),
        &["config", "user.email", "recovery-fixture@example.invalid"],
    );
    fs::write(repo.join("base.txt"), "clean base\n").unwrap();
    git(Some(&repo), &["add", "base.txt"]);
    git(Some(&repo), &["commit", "--quiet", "-m", "base"]);
    git(
        Some(&repo),
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(Some(&repo), &["push", "--quiet", "-u", "origin", "main"]);

    git(Some(&repo), &["switch", "--quiet", "-c", "safe-feature"]);
    fs::write(repo.join("safe.txt"), "safe recovery data\n").unwrap();
    git(Some(&repo), &["add", "safe.txt"]);
    git(Some(&repo), &["commit", "--quiet", "-m", "safe branch"]);
    let safe_oid = git_text(Some(&repo), &["rev-parse", "HEAD"]);

    git(Some(&repo), &["switch", "--quiet", "main"]);
    git(Some(&repo), &["switch", "--quiet", "-c", "unsafe-feature"]);
    fs::write(
        repo.join("credential.txt"),
        "github_pat_1234567890abcdefghijklmnopqrstuvwxyz\n",
    )
    .unwrap();
    git(Some(&repo), &["add", "credential.txt"]);
    git(Some(&repo), &["commit", "--quiet", "-m", "unsafe branch"]);
    let unsafe_oid = git_text(Some(&repo), &["rev-parse", "HEAD"]);

    let scan = cli(
        root,
        &remote,
        &state,
        &["scan", "--roots", root.to_str().unwrap()],
    );
    assert_cli_ok(&scan, "scan");
    let preserve = cli(root, &remote, &state, &["preserve"]);
    assert_cli_ok(&preserve, "preserve");

    let repo_row = &manifest(&state)["repositories"][0];
    assert_eq!(repo_row["preservation"], "blocked", "{repo_row}");
    let saved = repo_row["saved"].as_array().unwrap();
    let safe = saved
        .iter()
        .find(|item| item["name"] == "branch:safe-feature")
        .expect("safe branch should be represented");
    assert_eq!(safe["commit"], safe_oid);
    assert_eq!(safe["verification"], "push-succeeded");
    assert!(
        !saved
            .iter()
            .any(|item| item["name"] == "branch:unsafe-feature"),
        "secret-bearing branch must not enter saved refs: {saved:?}"
    );
    assert_eq!(
        git_text(
            None,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                &format!("refs/heads/{}", safe["remote_ref"].as_str().unwrap()),
            ],
        ),
        safe_oid
    );
    assert!(
        !git_text(
            None,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/recovery/",
            ],
        )
        .lines()
        .any(|reference| reference.contains("unsafe-feature")),
        "unsafe branch has no recovery ref"
    );
    let unsafe_object = Command::new("git")
        .args([
            "--git-dir",
            remote.to_str().unwrap(),
            "cat-file",
            "-e",
            &unsafe_oid,
        ])
        .output()
        .expect("inspect remote object database");
    assert!(
        !unsafe_object.status.success(),
        "secret-bearing commit was uploaded to the remote object database"
    );
}
