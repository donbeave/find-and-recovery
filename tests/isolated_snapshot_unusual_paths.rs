#[path = "../src/isolated_snapshot.rs"]
mod isolated_snapshot;

use isolated_snapshot::snapshot_worktree;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

#[test]
fn repeated_snapshot_keeps_newline_and_tab_paths_and_distinct_dirty_trees() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, ["init", "-q", "-b", "main"]);
    git(&repo, ["config", "user.name", "Snapshot Test"]);
    git(&repo, ["config", "user.email", "snapshot-test@localhost"]);

    fs::write(repo.join("tracked.txt"), "base\n").unwrap();
    git(&repo, ["add", "tracked.txt"]);
    git(&repo, ["commit", "-qm", "base"]);

    let unusual_name = "tab\tand-newline\n.txt";
    fs::write(repo.join(unusual_name), "untracked unusual path\n").unwrap();
    fs::write(repo.join("tracked.txt"), "staged content\n").unwrap();
    git(&repo, ["add", "tracked.txt"]);
    fs::write(repo.join("tracked.txt"), "unstaged content\n").unwrap();

    let staged = git_text(&repo, ["write-tree"]);
    let expected_worktree_index = temp.path().join("expected-worktree-index");
    git_with_index(&repo, &expected_worktree_index, ["read-tree", "HEAD"]);
    git_with_index(&repo, &expected_worktree_index, ["add", "-A", "--", "."]);
    let expected_worktree = git_text_with_index(&repo, &expected_worktree_index, ["write-tree"]);
    let first = snapshot_worktree(&repo).expect("snapshot must accept NUL-delimited unusual paths");
    let second = snapshot_worktree(&repo).expect("repeated snapshot must succeed");

    assert_eq!(
        first, second,
        "identical dirty state must snapshot idempotently"
    );
    assert_eq!(first.staged_tree_oid, staged);
    assert_ne!(
        first.staged_tree_oid, first.worktree_tree_oid,
        "staged index and complete worktree must remain distinct"
    );
    assert_eq!(first.worktree_tree_oid, expected_worktree);
    assert_eq!(
        blob_at_tree(&repo, &first.staged_tree_oid, "tracked.txt").as_deref(),
        Some(b"staged content\n".as_slice()),
        "staged tree must retain the index version"
    );
    assert_eq!(
        blob_at_tree(&repo, &first.worktree_tree_oid, "tracked.txt").as_deref(),
        Some(b"unstaged content\n".as_slice()),
        "worktree tree must retain the later working version"
    );
    assert_eq!(
        blob_at_tree(&repo, &first.worktree_tree_oid, unusual_name).as_deref(),
        Some(b"untracked unusual path\n".as_slice()),
        "worktree tree must retain an untracked path containing tab and newline"
    );
    assert_eq!(
        blob_at_tree(&repo, &first.staged_tree_oid, unusual_name),
        None,
        "untracked content must not leak into the staged tree"
    );
}

#[test]
fn missing_index_blob_fails_closed_before_reporting_snapshot_trees() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, ["init", "-q", "-b", "main"]);
    git(&repo, ["config", "user.name", "Snapshot Test"]);
    git(&repo, ["config", "user.email", "snapshot-test@localhost"]);

    fs::write(repo.join("tracked.txt"), "tracked payload\n").unwrap();
    git(&repo, ["add", "tracked.txt"]);
    git(&repo, ["commit", "-qm", "base"]);

    let blob = git_text(&repo, ["rev-parse", "HEAD:tracked.txt"]);
    let object_path = repo
        .join(git_text(&repo, ["rev-parse", "--git-path", "objects"]))
        .join(&blob[..2])
        .join(&blob[2..]);
    fs::remove_file(object_path).unwrap();

    let error = snapshot_worktree(&repo)
        .expect_err("snapshot must not succeed with a missing indexed blob");
    assert!(
        error.contains("write isolated staged tree failed"),
        "missing indexed blob should fail while validating the staged tree: {error}"
    );
}

fn blob_at_tree(repo: &Path, tree: &str, path: &str) -> Option<Vec<u8>> {
    let listing = git_output(repo, ["ls-tree", "-rz", "--full-tree", tree]);
    for entry in listing
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let separator = entry.iter().position(|byte| *byte == b'\t')?;
        if &entry[separator + 1..] != path.as_bytes() {
            continue;
        }
        let metadata = std::str::from_utf8(&entry[..separator]).ok()?;
        let object = metadata.split_ascii_whitespace().nth(2)?;
        return Some(git_output(repo, ["cat-file", "blob", object]));
    }
    None
}

fn git(repo: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) {
    let output = run_git(repo, args);
    assert!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_text(repo: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> String {
    String::from_utf8(git_output(repo, args))
        .unwrap()
        .trim()
        .to_owned()
}

fn git_with_index(repo: &Path, index: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git command with isolated index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_text_with_index(
    repo: &Path,
    index: &Path,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git command with isolated index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git_output(repo: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Vec<u8> {
    let output = run_git(repo, args);
    assert!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn run_git(repo: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .unwrap()
}
