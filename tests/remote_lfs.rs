#[path = "../src/lfs_batch.rs"]
mod lfs_batch;
#[path = "../src/remote_lfs.rs"]
mod remote_lfs;

use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Fixture {
    _temp: tempfile::TempDir,
    remote: PathBuf,
    lfs_store: PathBuf,
    work: PathBuf,
    reference: String,
    oid: String,
    payload: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        assert!(
            Command::new("git")
                .args(["lfs", "version"])
                .output()
                .unwrap()
                .status
                .success(),
            "Git LFS is required"
        );
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        let lfs_store = temp.path().join("lfs-store");
        let work = temp.path().join("work");
        git(None, &["init", "--bare", "--quiet", p(&remote)]);
        git(None, &["init", "--bare", "--quiet", p(&lfs_store)]);
        git(None, &["init", "--quiet", "-b", "main", p(&work)]);
        git(Some(&work), &["config", "user.name", "LFS fixture"]);
        git(
            Some(&work),
            &["config", "user.email", "lfs@example.invalid"],
        );
        git(Some(&work), &["lfs", "install", "--local"]);

        let endpoint = format!("file://{}", lfs_store.display());
        let lfsconfig = format!("[lfs]\n\turl = {endpoint}\n");
        fs::write(work.join(".lfsconfig"), lfsconfig).unwrap();
        let payload = (0..1024 * 32)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(work.join("payload.bin"), &payload).unwrap();
        git(Some(&work), &["lfs", "track", "*.bin"]);
        git(
            Some(&work),
            &["add", ".lfsconfig", ".gitattributes", "payload.bin"],
        );
        git(Some(&work), &["commit", "--quiet", "-m", "LFS fixture"]);
        git(Some(&work), &["remote", "add", "origin", p(&remote)]);
        git(
            Some(&work),
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        );
        let oid = git(Some(&work), &["lfs", "ls-files", "--long"])
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        Self {
            _temp: temp,
            remote,
            lfs_store,
            work,
            reference: "refs/heads/main".into(),
            oid,
            payload,
        }
    }

    fn remove_payload(&self) {
        let path = self.remote_lfs_path();
        fs::remove_file(path).unwrap();
    }

    fn corrupt_payload(&self) {
        let path = self.remote_lfs_path();
        fs::write(path, b"corrupted LFS bytes").unwrap();
    }

    fn remove_lfs_file_at_tip(&self) {
        git(Some(&self.work), &["rm", "--quiet", "payload.bin"]);
        git(
            Some(&self.work),
            &["commit", "--quiet", "-m", "remove LFS file"],
        );
        git(
            Some(&self.work),
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        );
    }

    fn remote_lfs_path(&self) -> PathBuf {
        self.lfs_store
            .join("lfs")
            .join("objects")
            .join(&self.oid[..2])
            .join(&self.oid[2..4])
            .join(&self.oid)
    }

    fn local_lfs_path(&self) -> PathBuf {
        self.work
            .join(".git")
            .join("lfs")
            .join("objects")
            .join(&self.oid[..2])
            .join(&self.oid[2..4])
            .join(&self.oid)
    }

    fn expected_object(&self) -> remote_lfs::LfsObject {
        remote_lfs::LfsObject {
            oid: self.oid.clone(),
            size: self.payload.len() as u64,
        }
    }

    fn add_orphan_local_payload(&self, payload: &[u8]) -> String {
        let oid = format!("{:x}", Sha256::digest(payload));
        let path = self
            .work
            .join(".git")
            .join("lfs")
            .join("objects")
            .join(&oid[..2])
            .join(&oid[2..4])
            .join(&oid);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, payload).unwrap();
        oid
    }
}

#[test]
fn fresh_verifier_fetches_and_checks_actual_lfs_bytes() {
    let fixture = Fixture::new();
    assert!(fixture.remote_lfs_path().is_file());
    let objects = remote_lfs::verify_remote_lfs_refs(
        p(&fixture.remote),
        std::slice::from_ref(&fixture.reference),
    )
    .unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].oid, fixture.oid);
    assert_eq!(objects[0].size, fixture.payload.len() as u64);
}

#[test]
fn fresh_verifier_requires_exact_saved_lfs_pointer_set() {
    let fixture = Fixture::new();
    remote_lfs::verify_remote_lfs_refs_match(
        p(&fixture.remote),
        std::slice::from_ref(&fixture.reference),
        &[fixture.expected_object()],
    )
    .unwrap();
    let error = remote_lfs::verify_remote_lfs_refs_match(
        p(&fixture.remote),
        std::slice::from_ref(&fixture.reference),
        &[],
    )
    .unwrap_err();
    assert!(error.contains("pointer set differs"), "{error}");
}

#[test]
fn local_payload_validation_accepts_bytes_matching_pointer() {
    let fixture = Fixture::new();
    let objects = remote_lfs::inventory_local_lfs_objects(
        &fixture.work,
        std::slice::from_ref(&fixture.reference),
    )
    .unwrap();
    assert_eq!(objects, vec![fixture.expected_object()]);
    remote_lfs::validate_local_lfs_payloads(&fixture.work, &objects).unwrap();
}

#[test]
fn local_payload_validation_blocks_unreferenced_private_lfs_payloads() {
    let fixture = Fixture::new();
    let orphan_oid = fixture.add_orphan_local_payload(b"orphaned local LFS payload");
    let error =
        remote_lfs::validate_local_lfs_payloads(&fixture.work, &[fixture.expected_object()])
            .unwrap_err();
    assert!(
        error.contains("unreferenced local Git LFS payload"),
        "{error}"
    );
    assert!(error.contains(&orphan_oid), "{error}");
}

#[test]
fn local_inventory_includes_pointer_from_ancestor_removed_at_tip() {
    let fixture = Fixture::new();
    fixture.remove_lfs_file_at_tip();
    let objects = remote_lfs::inventory_local_lfs_objects(
        &fixture.work,
        std::slice::from_ref(&fixture.reference),
    )
    .unwrap();
    assert_eq!(objects, vec![fixture.expected_object()]);

    let tip = git(Some(&fixture.work), &["rev-parse", "HEAD"]);
    let saved = remote_lfs::inventory_local_lfs_commits(&fixture.work, &[tip]).unwrap();
    assert_eq!(saved, vec![fixture.expected_object()]);
    remote_lfs::validate_local_lfs_payloads(&fixture.work, &saved).unwrap();
}

#[test]
fn local_inventory_ignores_replacement_refs_when_reading_original_history() {
    let fixture = Fixture::new();
    let original = git(Some(&fixture.work), &["rev-parse", "HEAD"]);
    fixture.remove_lfs_file_at_tip();
    let tree = git(Some(&fixture.work), &["rev-parse", "HEAD^{tree}"]);
    let replacement = git(
        Some(&fixture.work),
        &[
            "commit-tree",
            &tree,
            "-m",
            "replacement without LFS pointer",
        ],
    );
    git(Some(&fixture.work), &["replace", &original, &replacement]);

    let objects =
        remote_lfs::inventory_local_lfs_commits(&fixture.work, std::slice::from_ref(&original))
            .unwrap();
    assert_eq!(objects, vec![fixture.expected_object()]);
}

#[test]
fn local_inventory_includes_lfs_pointer_from_stash_untracked_parent() {
    let fixture = Fixture::new();
    let stash_payload = b"untracked stash LFS payload".repeat(128);
    fs::write(fixture.work.join("stash-only.bin"), &stash_payload).unwrap();
    git(
        Some(&fixture.work),
        &[
            "stash",
            "push",
            "--include-untracked",
            "--quiet",
            "-m",
            "LFS stash",
        ],
    );
    let stash = git(Some(&fixture.work), &["rev-parse", "refs/stash"]);

    let objects =
        remote_lfs::inventory_local_lfs_commits(&fixture.work, std::slice::from_ref(&stash))
            .unwrap();
    assert!(objects.contains(&fixture.expected_object()));
    assert!(
        objects
            .iter()
            .any(|object| object.size == stash_payload.len() as u64)
    );
    remote_lfs::validate_local_lfs_payloads(&fixture.work, &objects).unwrap();
}

#[test]
fn local_inventory_includes_lfs_pointer_from_detached_commit_history() {
    let fixture = Fixture::new();
    let detached_payload = b"detached history LFS payload".repeat(128);
    git(
        Some(&fixture.work),
        &["checkout", "--detach", "--quiet", "HEAD"],
    );
    fs::write(fixture.work.join("detached-only.bin"), &detached_payload).unwrap();
    git(Some(&fixture.work), &["add", "detached-only.bin"]);
    git(
        Some(&fixture.work),
        &["commit", "--quiet", "-m", "detached LFS history"],
    );
    let detached_head = git(Some(&fixture.work), &["rev-parse", "HEAD"]);

    let objects = remote_lfs::inventory_local_lfs_commits(
        &fixture.work,
        std::slice::from_ref(&detached_head),
    )
    .unwrap();
    assert!(objects.contains(&fixture.expected_object()));
    assert!(
        objects
            .iter()
            .any(|object| object.size == detached_payload.len() as u64)
    );
    remote_lfs::validate_local_lfs_payloads(&fixture.work, &objects).unwrap();
}

#[test]
fn local_inventory_blocks_shallow_history_that_can_hide_ancestor_pointers() {
    let fixture = Fixture::new();
    fixture.remove_lfs_file_at_tip();
    let clone = fixture._temp.path().join("shallow-clone");
    let remote_url = format!("file://{}", fixture.remote.display());
    git(
        None,
        &["clone", "--quiet", "--depth", "1", &remote_url, p(&clone)],
    );
    let head = git(Some(&clone), &["rev-parse", "HEAD"]);

    let error = remote_lfs::inventory_local_lfs_commits(&clone, &[head]).unwrap_err();
    assert!(
        error.contains("shallow Git history is incomplete"),
        "{error}"
    );
}

#[test]
fn local_inventory_blocks_grafts_that_change_the_history_view() {
    let fixture = Fixture::new();
    fs::write(fixture.work.join(".git/info/grafts"), b"history override\n").unwrap();
    let head = git(Some(&fixture.work), &["rev-parse", "HEAD"]);

    let error = remote_lfs::inventory_local_lfs_commits(&fixture.work, &[head]).unwrap_err();
    assert!(error.contains("Git grafts alter commit history"), "{error}");
}

#[test]
fn fresh_verifier_downloads_lfs_from_detached_recovery_history() {
    let fixture = Fixture::new();
    let detached_payload = b"detached remote LFS payload".repeat(128);
    git(
        Some(&fixture.work),
        &["checkout", "--detach", "--quiet", "HEAD"],
    );
    fs::write(fixture.work.join("detached-only.bin"), &detached_payload).unwrap();
    git(Some(&fixture.work), &["add", "detached-only.bin"]);
    git(
        Some(&fixture.work),
        &["commit", "--quiet", "-m", "detached LFS history"],
    );
    git(
        Some(&fixture.work),
        &[
            "push",
            "--quiet",
            "origin",
            "HEAD:refs/heads/recovery/detached",
        ],
    );
    git(Some(&fixture.work), &["rm", "--quiet", "detached-only.bin"]);
    git(
        Some(&fixture.work),
        &["commit", "--quiet", "-m", "remove detached LFS file"],
    );
    git(
        Some(&fixture.work),
        &[
            "push",
            "--quiet",
            "origin",
            "HEAD:refs/heads/recovery/detached",
        ],
    );

    let reference = "refs/heads/recovery/detached".to_owned();
    let objects = remote_lfs::verify_remote_lfs_refs(p(&fixture.remote), &[reference]).unwrap();
    assert!(objects.contains(&fixture.expected_object()));
    assert!(
        objects
            .iter()
            .any(|object| object.size == detached_payload.len() as u64)
    );
}

#[test]
fn fresh_remote_verifier_fetches_and_checks_ancestor_pointer_removed_at_tip() {
    let fixture = Fixture::new();
    fixture.remove_lfs_file_at_tip();
    assert!(fixture.remote_lfs_path().is_file());
    let objects = remote_lfs::verify_remote_lfs_refs(
        p(&fixture.remote),
        std::slice::from_ref(&fixture.reference),
    )
    .unwrap();
    assert_eq!(objects, vec![fixture.expected_object()]);
}

#[test]
fn fresh_remote_verifier_rejects_corrupt_ancestor_pointer_removed_at_tip() {
    let fixture = Fixture::new();
    fixture.remove_lfs_file_at_tip();
    fixture.corrupt_payload();
    let error = remote_lfs::verify_remote_lfs_refs(
        p(&fixture.remote),
        std::slice::from_ref(&fixture.reference),
    )
    .unwrap_err();
    assert!(
        error.contains("SHA-256 mismatch")
            || error.contains("size mismatch")
            || error.contains("fetch LFS payloads"),
        "{error}"
    );
}

#[test]
fn inventory_finds_lfs_pointer_added_only_to_unreferenced_snapshot_commit() {
    let fixture = Fixture::new();
    let snapshot_payload = b"new worktree snapshot LFS payload".repeat(128);
    fs::write(fixture.work.join("snapshot.bin"), &snapshot_payload).unwrap();
    git(Some(&fixture.work), &["lfs", "track", "snapshot.bin"]);
    git(
        Some(&fixture.work),
        &["add", ".gitattributes", "snapshot.bin"],
    );
    let tree = git(Some(&fixture.work), &["write-tree"]);
    let snapshot = git(
        Some(&fixture.work),
        &["commit-tree", &tree, "-p", "HEAD", "-m", "snapshot"],
    );
    let refs = git(Some(&fixture.work), &["show-ref"]);
    assert!(!refs.lines().any(|line| line.starts_with(&snapshot)));
    let objects =
        remote_lfs::inventory_local_lfs_commits(&fixture.work, std::slice::from_ref(&snapshot))
            .unwrap();
    assert_eq!(objects.len(), 2);
    assert!(
        objects
            .iter()
            .any(|object| object.size == snapshot_payload.len() as u64)
    );
    remote_lfs::validate_local_lfs_payloads(&fixture.work, &objects).unwrap();
}

#[test]
fn local_payload_validation_rejects_missing_and_corrupt_bytes() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.local_lfs_path()).unwrap();
    let error =
        remote_lfs::validate_local_lfs_payloads(&fixture.work, &[fixture.expected_object()])
            .unwrap_err();
    assert!(
        error.contains("missing downloaded Git LFS payload"),
        "{error}"
    );

    let fixture = Fixture::new();
    fs::write(fixture.local_lfs_path(), b"corrupted local bytes").unwrap();
    let error =
        remote_lfs::validate_local_lfs_payloads(&fixture.work, &[fixture.expected_object()])
            .unwrap_err();
    assert!(error.contains("SHA-256 mismatch"), "{error}");
}

#[test]
fn validated_payload_can_be_uploaded_as_one_batch() {
    let fixture = Fixture::new();
    fixture.remove_payload();
    let objects = remote_lfs::inventory_local_lfs_objects(
        &fixture.work,
        std::slice::from_ref(&fixture.reference),
    )
    .unwrap();
    remote_lfs::validate_local_lfs_payloads(&fixture.work, &objects).unwrap();
    let payloads = objects
        .iter()
        .map(|object| lfs_batch::LfsPayload {
            oid: object.oid.clone(),
            size: object.size,
        })
        .collect::<Vec<_>>();
    lfs_batch::push_lfs_payloads(&fixture.work, "origin", &payloads).unwrap();
    assert!(fixture.remote_lfs_path().is_file());
    remote_lfs::verify_remote_lfs_refs(
        p(&fixture.remote),
        std::slice::from_ref(&fixture.reference),
    )
    .unwrap();
}

#[test]
fn invalid_local_bytes_block_the_batch_upload() {
    let fixture = Fixture::new();
    fixture.remove_payload();
    fs::write(fixture.local_lfs_path(), b"corrupt local bytes").unwrap();
    let error = lfs_batch::push_lfs_payloads(
        &fixture.work,
        "origin",
        &[lfs_batch::LfsPayload {
            oid: fixture.oid.clone(),
            size: fixture.payload.len() as u64,
        }],
    )
    .unwrap_err();
    assert!(error.contains("SHA-256 mismatch"), "{error}");
    assert!(!fixture.remote_lfs_path().exists());
}

#[test]
fn missing_remote_payload_fails_closed() {
    let fixture = Fixture::new();
    fixture.remove_payload();
    let error = remote_lfs::verify_remote_lfs_refs(
        p(&fixture.remote),
        std::slice::from_ref(&fixture.reference),
    )
    .unwrap_err();
    assert!(error.contains("fetch LFS payloads"), "{error}");
}

#[test]
fn corrupt_remote_payload_fails_closed() {
    let fixture = Fixture::new();
    fixture.corrupt_payload();
    let error = remote_lfs::verify_remote_lfs_refs(
        p(&fixture.remote),
        std::slice::from_ref(&fixture.reference),
    )
    .unwrap_err();
    assert!(
        error.contains("SHA-256 mismatch")
            || error.contains("size mismatch")
            || error.contains("fetch LFS payloads"),
        "{error}"
    );
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
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
