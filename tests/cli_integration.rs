use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
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

fn cli(temp_root: &Path, remote: &Path, state: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_find-and-recovery"));
    command
        .args(["--remote", remote.to_str().unwrap()])
        .args(["--state", state.to_str().unwrap()])
        .args(args)
        // Keep the CLI's isolated verification repos and scanner report inside
        // this disposable fixture too.
        .env("TMPDIR", temp_root);
    command.output().expect("start find-and-recovery")
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

fn manifest(state: &Path) -> Value {
    serde_json::from_slice(&fs::read(state.join("manifest.json")).unwrap()).unwrap()
}

fn ref_rows(git_dir: &Path, prefix: &str) -> Vec<(String, String)> {
    git_text(
        None,
        &[
            "--git-dir",
            git_dir.to_str().unwrap(),
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            prefix,
        ],
    )
    .lines()
    .map(|line| {
        let (reference, oid) = line.split_once(' ').expect("ref row");
        (reference.to_owned(), oid.to_owned())
    })
    .collect()
}

fn restored_file(cwd: &Path, reference: &str, path: &str) -> Option<String> {
    let spec = format!("{reference}:{path}");
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["show", &spec])
        .output()
        .expect("start git show");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn clone_fixture_repo(path: &Path, remote: &Path) {
    git(
        None,
        &[
            "clone",
            "--quiet",
            remote.to_str().unwrap(),
            path.to_str().unwrap(),
        ],
    );
}

#[test]
fn explicit_target_is_preserved_verified_cleaned_and_restorable_from_remote_alone() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let remote = root.join("selected-project.git");
    let foreign_remote = root.join("same-history-fork.git");
    let seed = root.join("seed");
    let copies = root.join("copies");
    let clone = copies.join("selected project");
    let worktree = copies.join("linked checkout");
    let decoy = copies.join("selected project fork");
    let state = root.join("audit state");
    let sentinel = root.join("unrelated sentinel.txt");
    fs::create_dir_all(&copies).unwrap();
    fs::write(&sentinel, "outside every candidate\n").unwrap();

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
            seed.to_str().unwrap(),
        ],
    );
    git(Some(&seed), &["config", "user.name", "CLI fixture"]);
    git(
        Some(&seed),
        &["config", "user.email", "cli-fixture@example.invalid"],
    );
    fs::write(seed.join("tracked.txt"), "base state\n").unwrap();
    git(Some(&seed), &["add", "tracked.txt"]);
    git(Some(&seed), &["commit", "--quiet", "-m", "base state"]);
    let main_oid = git_text(Some(&seed), &["rev-parse", "HEAD"]);
    git(
        Some(&seed),
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(Some(&seed), &["push", "--quiet", "-u", "origin", "main"]);

    // This second bare remote contains the same main commit. Its clone has the
    // same path stem and history, but belongs to a different selected target.
    git(
        None,
        &[
            "clone",
            "--quiet",
            "--bare",
            remote.to_str().unwrap(),
            foreign_remote.to_str().unwrap(),
        ],
    );
    fs::remove_dir_all(&seed).unwrap();
    clone_fixture_repo(&decoy, &foreign_remote);
    assert_eq!(git_text(Some(&decoy), &["rev-parse", "HEAD"]), main_oid);

    clone_fixture_repo(&clone, &remote);
    git(Some(&clone), &["config", "user.name", "CLI fixture"]);
    git(
        Some(&clone),
        &["config", "user.email", "cli-fixture@example.invalid"],
    );
    git(Some(&clone), &["switch", "--quiet", "-c", "feature"]);
    fs::write(clone.join("feature.txt"), "local-only feature commit\n").unwrap();
    git(Some(&clone), &["add", "feature.txt"]);
    git(
        Some(&clone),
        &["commit", "--quiet", "-m", "local-only feature"],
    );
    let feature_oid = git_text(Some(&clone), &["rev-parse", "HEAD"]);
    git(Some(&clone), &["switch", "--quiet", "main"]);
    git(
        Some(&clone),
        &[
            "worktree",
            "add",
            "--quiet",
            worktree.to_str().unwrap(),
            "feature",
        ],
    );

    fs::write(worktree.join("staged-only.txt"), "staged state\n").unwrap();
    git(Some(&worktree), &["add", "staged-only.txt"]);
    fs::write(worktree.join("tracked.txt"), "unstaged state\n").unwrap();
    fs::write(worktree.join("untracked.txt"), "untracked state\n").unwrap();

    let scan = cli(
        root,
        &remote,
        &state,
        &["scan", "--roots", copies.to_str().unwrap()],
    );
    assert_cli_ok(&scan, "scan");
    let discovered = manifest(&state);
    let repositories = discovered["repositories"].as_array().unwrap();
    let canonical_worktree = fs::canonicalize(&worktree).unwrap();
    assert_eq!(
        repositories.len(),
        1,
        "explicit remote identity must exclude a same-history fork: {discovered}"
    );
    assert_eq!(
        PathBuf::from(repositories[0]["path"].as_str().unwrap()),
        fs::canonicalize(&clone).unwrap()
    );
    assert!(
        repositories[0]["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|worktree_row| worktree_row["path"] == canonical_worktree.to_str().unwrap()),
        "linked checkout missing from owner inventory: {}",
        repositories[0]
    );

    let preserve = cli(root, &remote, &state, &["preserve"]);
    assert_cli_ok(&preserve, "preserve");
    let preserved = manifest(&state);
    let repository = &preserved["repositories"][0];
    assert_eq!(repository["preservation"], "complete", "{repository}");
    let saved = repository["saved"].as_array().unwrap();
    assert!(saved.iter().any(|item| {
        item["source"] == "branch"
            && item["name"] == "branch:feature"
            && item["commit"] == feature_oid
    }));
    assert!(saved.iter().any(|item| item["source"] == "staged-snapshot"));
    assert!(
        saved
            .iter()
            .any(|item| item["source"] == "worktree-snapshot")
    );
    assert_eq!(
        git_text(
            None,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main",
            ],
        ),
        main_oid,
        "preservation must leave the protected default branch untouched"
    );

    let verify = cli(root, &remote, &state, &["verify"]);
    assert_cli_ok(&verify, "verify");
    let verified = manifest(&state);
    let repository = &verified["repositories"][0];
    assert_eq!(
        repository["verification"], "isolated-verified",
        "{repository}"
    );
    assert!(repository["saved"].as_array().unwrap().iter().all(|item| {
        item["verification"] == "isolated-verified"
            && git_text(
                None,
                &[
                    "--git-dir",
                    remote.to_str().unwrap(),
                    "rev-parse",
                    &format!("refs/heads/{}", item["remote_ref"].as_str().unwrap()),
                ],
            ) == item["commit"].as_str().unwrap()
    }));

    let preview = cli(root, &remote, &state, &["preview"]);
    assert_cli_ok(&preview, "preview");
    assert_eq!(manifest(&state)["repositories"][0]["deletion"], "eligible");

    let cleanup = cli(root, &remote, &state, &["cleanup", "--execute"]);
    assert_cli_ok(&cleanup, "cleanup --execute");
    let cleaned = manifest(&state);
    assert_eq!(cleaned["repositories"][0]["deletion"], "deleted");
    assert!(!clone.exists(), "verified selected clone was retained");
    assert!(!worktree.exists(), "verified linked checkout was retained");
    assert!(decoy.is_dir(), "same-history foreign clone was removed");
    assert!(
        !seed.exists(),
        "temporary fixture seed unexpectedly reappeared"
    );
    assert!(remote.is_dir(), "remote repository was removed");
    assert!(state.is_dir(), "audit state was removed");
    assert_eq!(
        fs::read_to_string(&sentinel).unwrap(),
        "outside every candidate\n"
    );
    assert_eq!(
        git_text(
            None,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "symbolic-ref",
                "HEAD",
            ],
        ),
        "refs/heads/main"
    );
    assert_eq!(
        git_text(
            None,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main",
            ],
        ),
        main_oid
    );
    assert!(
        !Command::new("git")
            .args([
                "--git-dir",
                remote.to_str().unwrap(),
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/feature",
            ])
            .status()
            .unwrap()
            .success(),
        "local feature branch was unexpectedly pushed under its original name"
    );

    // A repeated destructive command must be a no-op after the successful run.
    let manifest_bytes_after_cleanup = fs::read(state.join("manifest.json")).unwrap();
    let remote_refs_before_retry = ref_rows(&remote, "refs/heads");
    let repeated_cleanup = cli(root, &remote, &state, &["cleanup", "--execute"]);
    assert_cli_ok(&repeated_cleanup, "repeated cleanup --execute");
    assert_eq!(
        fs::read(state.join("manifest.json")).unwrap(),
        manifest_bytes_after_cleanup,
        "repeat cleanup changed the completed audit record"
    );
    assert_eq!(remote_refs_before_retry, ref_rows(&remote, "refs/heads"));
    assert!(!clone.exists());
    assert!(!worktree.exists());
    assert!(decoy.is_dir());

    // Save only the observed remote refs, then discard the machine-local
    // manifest before reconstructing. The restore below reads the bare remote.
    let remote_recovery_refs = ref_rows(&remote, "refs/heads/recovery/find-and-recovery");
    assert!(!remote_recovery_refs.is_empty());
    assert!(
        remote_recovery_refs
            .iter()
            .any(|(_, oid)| oid == &feature_oid)
    );
    fs::remove_dir_all(&state).unwrap();

    let restored = root.join("remote-only restore");
    git(
        None,
        &[
            "clone",
            "--no-local",
            "--quiet",
            remote.to_str().unwrap(),
            restored.to_str().unwrap(),
        ],
    );
    assert!(
        !restored.join(".git/objects/info/alternates").exists(),
        "remote-only restore unexpectedly uses an object alternate"
    );
    git(
        Some(&restored),
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "origin",
            "+refs/heads/recovery/find-and-recovery/*:refs/restore/recovery/find-and-recovery/*",
        ],
    );
    let local_recovery_refs = git_text(
        Some(&restored),
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/restore/recovery/find-and-recovery",
        ],
    );
    let local_refs = local_recovery_refs
        .lines()
        .map(|line| {
            let (reference, oid) = line.split_once(' ').expect("restored ref row");
            (reference.to_owned(), oid.to_owned())
        })
        .collect::<Vec<_>>();
    assert_eq!(local_refs.len(), remote_recovery_refs.len());
    for (remote_ref, remote_oid) in &remote_recovery_refs {
        let local_ref = remote_ref
            .strip_prefix("refs/heads/")
            .map(|suffix| format!("refs/restore/{suffix}"))
            .unwrap();
        assert!(local_refs.contains(&(local_ref, remote_oid.clone())));
    }
    git(
        Some(&restored),
        &["fsck", "--full", "--strict", "--no-reflogs"],
    );
    assert!(local_refs.iter().any(|(_, oid)| oid == &feature_oid));
    let restored_refs = local_refs
        .iter()
        .map(|(reference, _)| reference.as_str())
        .collect::<Vec<_>>();
    assert!(
        restored_refs.iter().any(|reference| {
            restored_file(&restored, reference, "staged-only.txt").as_deref()
                == Some("staged state\n")
                && restored_file(&restored, reference, "tracked.txt").as_deref()
                    == Some("base state\n")
                && restored_file(&restored, reference, "untracked.txt").is_none()
        }),
        "staged version was not independently recoverable from remote refs"
    );
    assert!(
        restored_refs.iter().any(|reference| {
            restored_file(&restored, reference, "staged-only.txt").as_deref()
                == Some("staged state\n")
                && restored_file(&restored, reference, "tracked.txt").as_deref()
                    == Some("unstaged state\n")
                && restored_file(&restored, reference, "untracked.txt").as_deref()
                    == Some("untracked state\n")
        }),
        "full worktree version was not independently recoverable from remote refs"
    );
    assert_eq!(
        git_text(Some(&restored), &["show", "refs/heads/main:tracked.txt"]),
        "base state"
    );
}

#[test]
fn exact_temporary_path_opt_in_is_persisted_and_does_not_authorize_sibling_or_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let remote = root.join("target.git");
    let seed = root.join("seed");
    let fixture_root = root.join("recover-target-fixture");
    let selected = fixture_root.join("selected");
    let sibling = fixture_root.join("sibling");
    let state = root.join("state");
    fs::create_dir_all(&fixture_root).unwrap();
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
            seed.to_str().unwrap(),
        ],
    );
    git(Some(&seed), &["config", "user.name", "CLI fixture"]);
    git(
        Some(&seed),
        &["config", "user.email", "cli-fixture@example.invalid"],
    );
    fs::write(seed.join("tracked.txt"), "base\n").unwrap();
    git(Some(&seed), &["add", "tracked.txt"]);
    git(Some(&seed), &["commit", "--quiet", "-m", "base"]);
    git(
        Some(&seed),
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(Some(&seed), &["push", "--quiet", "-u", "origin", "main"]);
    fs::remove_dir_all(&seed).unwrap();
    clone_fixture_repo(&selected, &remote);
    clone_fixture_repo(&sibling, &remote);

    let scan = cli(
        root,
        &remote,
        &state,
        &[
            "scan",
            "--roots",
            fixture_root.to_str().unwrap(),
            "--allow-temp-repository",
            selected.to_str().unwrap(),
        ],
    );
    assert_cli_ok(&scan, "scan with exact temporary path opt-in");
    let scanned = manifest(&state);
    let repos = scanned["repositories"].as_array().unwrap();
    let selected_row = repos
        .iter()
        .find(|r| {
            r["path"]
                == fs::canonicalize(&selected)
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
        })
        .unwrap();
    let sibling_row = repos
        .iter()
        .find(|r| {
            r["path"]
                == fs::canonicalize(&sibling)
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
        })
        .unwrap();
    assert!(selected_row["temporary_path_authorization"].is_object());
    assert!(sibling_row["temporary_path_authorization"].is_null());

    let preserve = cli(root, &remote, &state, &["preserve"]);
    assert_cli_ok(&preserve, "preserve authorized path and block sibling");
    let saved = manifest(&state);
    let selected_row = saved["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| {
            r["path"]
                == fs::canonicalize(&selected)
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
        })
        .unwrap();
    let sibling_row = saved["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| {
            r["path"]
                == fs::canonicalize(&sibling)
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
        })
        .unwrap();
    assert_eq!(selected_row["preservation"], "complete", "{selected_row}");
    assert_eq!(sibling_row["preservation"], "blocked", "{sibling_row}");
    assert!(
        sibling_row["verification_error"]
            .as_str()
            .unwrap()
            .contains("temporary-recovery-fixture")
    );

    let mut tampered = saved;
    let selected_row = tampered["repositories"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| {
            r["path"]
                == fs::canonicalize(&selected)
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
        })
        .unwrap();
    selected_row["temporary_path_authorization"]["inode"] = serde_json::json!(0);
    fs::write(
        state.join("manifest.json"),
        serde_json::to_vec_pretty(&tampered).unwrap(),
    )
    .unwrap();
    let verify = cli(root, &remote, &state, &["verify"]);
    assert_cli_ok(&verify, "verify rejects changed authorized identity");
    let verified = manifest(&state);
    let selected_row = verified["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| {
            r["path"]
                == fs::canonicalize(&selected)
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
        })
        .unwrap();
    assert!(
        selected_row["verification_error"]
            .as_str()
            .unwrap()
            .contains("filesystem identity changed")
    );
}
