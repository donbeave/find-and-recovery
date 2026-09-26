#[path = "../src/remote_archive.rs"]
mod remote_archive;

use remote_archive::{ArchiveRecord, archive_commit, archive_ref_for};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    _temp: tempfile::TempDir,
    remote: PathBuf,
    work: PathBuf,
    oid: String,
    tree: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        let work = temp.path().join("work");
        run(
            None,
            &["init", "--bare", "--quiet", remote.to_str().unwrap()],
        );
        run(
            None,
            &["init", "--quiet", "-b", "main", work.to_str().unwrap()],
        );
        run(Some(&work), &["config", "user.name", "archive fixture"]);
        run(
            Some(&work),
            &["config", "user.email", "archive@example.invalid"],
        );
        fs::write(work.join("file.txt"), "archive fixture\n").unwrap();
        run(Some(&work), &["add", "file.txt"]);
        run(Some(&work), &["commit", "--quiet", "-m", "fixture"]);
        run(Some(&work), &["branch", "topic"]);
        push(&work, &remote, "HEAD", "refs/heads/topic");
        let oid = git(Some(&work), &["rev-parse", "HEAD"]);
        let tree = git(Some(&work), &["rev-parse", "HEAD^{tree}"]);
        Self {
            _temp: temp,
            remote,
            work,
            oid,
            tree,
        }
    }

    fn remote_arg(&self) -> &str {
        self.remote.to_str().unwrap()
    }
}

#[test]
fn new_archive_is_created_and_verified() {
    let fixture = Fixture::new();
    let archive = archive_commit(
        fixture.remote_arg(),
        "refs/heads/topic",
        &fixture.oid,
        &fixture.tree,
    )
    .unwrap();
    assert_eq!(archive, archive_ref_for(&fixture.oid));
    assert_eq!(
        remote_oid(&fixture.remote, &archive),
        Some(fixture.oid.clone())
    );
}

#[test]
fn repeated_same_oid_archive_is_idempotent() {
    let fixture = Fixture::new();
    let first = archive_commit(
        fixture.remote_arg(),
        "refs/heads/topic",
        &fixture.oid,
        &fixture.tree,
    )
    .unwrap();
    let second = archive_commit(
        fixture.remote_arg(),
        "refs/heads/topic",
        &fixture.oid,
        &fixture.tree,
    )
    .unwrap();
    assert_eq!(first, second);
    assert_eq!(remote_oid(&fixture.remote, &first), Some(fixture.oid));
}

#[test]
fn conflicting_existing_archive_ref_fails_closed() {
    let fixture = Fixture::new();
    let archive = archive_commit(
        fixture.remote_arg(),
        "refs/heads/topic",
        &fixture.oid,
        &fixture.tree,
    )
    .unwrap();

    fs::write(fixture.work.join("file.txt"), "new tip\n").unwrap();
    run(Some(&fixture.work), &["add", "file.txt"]);
    run(Some(&fixture.work), &["commit", "--quiet", "-m", "new tip"]);
    push(&fixture.work, &fixture.remote, "HEAD", "refs/heads/topic");
    let new_oid = git(Some(&fixture.work), &["rev-parse", "HEAD"]);
    let new_tree = git(Some(&fixture.work), &["rev-parse", "HEAD^{tree}"]);
    let conflicting_archive = archive_ref_for(&new_oid);
    push(
        &fixture.work,
        &fixture.remote,
        &fixture.oid,
        &conflicting_archive,
    );

    let error = archive_commit(
        fixture.remote_arg(),
        "refs/heads/topic",
        &new_oid,
        &new_tree,
    )
    .unwrap_err();
    assert!(error.contains("different commit"));
    assert_eq!(
        remote_oid(&fixture.remote, &archive),
        Some(fixture.oid.clone())
    );
    assert_eq!(
        remote_oid(&fixture.remote, &conflicting_archive),
        Some(fixture.oid)
    );
}

#[test]
fn bad_tree_is_rejected_before_archive_creation() {
    let fixture = Fixture::new();
    let error = archive_commit(
        fixture.remote_arg(),
        "refs/heads/topic",
        &fixture.oid,
        &"0".repeat(fixture.tree.len()),
    )
    .unwrap_err();
    assert!(error.contains("tree"));
    assert_eq!(
        remote_oid(&fixture.remote, &archive_ref_for(&fixture.oid)),
        None
    );
}

#[test]
fn bad_commit_is_rejected_before_archive_creation() {
    let fixture = Fixture::new();
    let error = archive_commit(
        fixture.remote_arg(),
        "refs/heads/topic",
        &"0".repeat(fixture.oid.len()),
        &fixture.tree,
    )
    .unwrap_err();
    assert!(error.contains("source branch tip"));
    assert_eq!(
        remote_oid(
            &fixture.remote,
            &archive_ref_for(&"0".repeat(fixture.oid.len()))
        ),
        None
    );
}

#[test]
fn batch_archives_aliases_with_one_ref_per_commit() {
    let fixture = Fixture::new();
    push(
        &fixture.work,
        &fixture.remote,
        "HEAD",
        "refs/heads/topic-alias",
    );
    let result = remote_archive::archive_commits_batch(
        fixture.remote_arg(),
        &[
            ArchiveRecord {
                original_ref: "refs/heads/topic".into(),
                expected_oid: fixture.oid.clone(),
                expected_tree: fixture.tree.clone(),
            },
            ArchiveRecord {
                original_ref: "refs/heads/topic-alias".into(),
                expected_oid: fixture.oid.clone(),
                expected_tree: fixture.tree.clone(),
            },
        ],
    )
    .unwrap();
    assert_eq!(result["refs/heads/topic"], result["refs/heads/topic-alias"]);
    assert_eq!(
        remote_oid(&fixture.remote, &result["refs/heads/topic"]),
        Some(fixture.oid)
    );
}

#[test]
fn archive_reads_source_from_remote_with_no_local_object_dependency() {
    let fixture = Fixture::new();
    let oid = fixture.oid.clone();
    let tree = fixture.tree.clone();
    let remote = fixture.remote.clone();
    let remote_arg = remote.to_str().unwrap().to_owned();
    fs::remove_dir_all(&fixture.work).unwrap();

    let archive = archive_commit(&remote_arg, "refs/heads/topic", &oid, &tree).unwrap();
    assert_eq!(remote_oid(&remote, &archive), Some(oid));
}

fn push(work: &Path, remote: &Path, source: &str, destination: &str) {
    let destination = format!("{source}:{destination}");
    run(
        Some(work),
        &["push", "--quiet", remote.to_str().unwrap(), &destination],
    );
}

fn remote_oid(remote: &Path, reference: &str) -> Option<String> {
    let output = Command::new("git")
        .args([
            "--git-dir",
            remote.to_str().unwrap(),
            "show-ref",
            "--verify",
            reference,
        ])
        .output()
        .unwrap();
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    })
}

fn git(cwd: Option<&Path>, args: &[&str]) -> String {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
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

fn run(cwd: Option<&Path>, args: &[&str]) {
    let _ = git(cwd, args);
}
