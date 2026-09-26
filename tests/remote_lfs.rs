#[path = "../src/lfs_batch.rs"]
mod lfs_batch;
#[path = "../src/remote_lfs.rs"]
mod remote_lfs;

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
