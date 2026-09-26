#[path = "../src/conditional_delete.rs"]
mod conditional_delete;

use conditional_delete::{
    BranchCandidate, RefAssertion, RefState, RetentionAnchor, ReviewedDeletionFacts,
    TransactionDisposition, ValidatedDestination, VerifiedDeletionScope,
    delete_branches_if_unchanged,
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const IDENTITY: &str = "fixture/repository";

struct Fixture {
    _temporary: tempfile::TempDir,
    remote: PathBuf,
    work: PathBuf,
    global_config: PathBuf,
    empty_hooks: PathBuf,
    old_oid: String,
}

impl Fixture {
    fn new(candidate_refs: &[String]) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let remote = temporary.path().join("remote.git");
        let work = temporary.path().join("work");
        let template = temporary.path().join("empty-template");
        let global_config = temporary.path().join("empty-global.gitconfig");
        let empty_hooks = temporary.path().join("empty-hooks");
        fs::create_dir(&template).unwrap();
        fs::create_dir(&empty_hooks).unwrap();
        fs::write(&global_config, b"").unwrap();

        git(
            None,
            &global_config,
            &empty_hooks,
            &[
                "init",
                "--bare",
                "-q",
                "--template",
                template.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        git(
            None,
            &global_config,
            &empty_hooks,
            &[
                "init",
                "-q",
                "--template",
                template.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        git(
            Some(&work),
            &global_config,
            &empty_hooks,
            &["config", "user.name", "conditional-delete fixture"],
        );
        git(
            Some(&work),
            &global_config,
            &empty_hooks,
            &["config", "user.email", "fixture@example.invalid"],
        );
        fs::write(work.join("file"), b"retained data\n").unwrap();
        git(Some(&work), &global_config, &empty_hooks, &["add", "file"]);
        git(
            Some(&work),
            &global_config,
            &empty_hooks,
            &["commit", "-q", "-m", "fixture commit"],
        );
        let old_oid = git_text(
            Some(&work),
            &global_config,
            &empty_hooks,
            &["rev-parse", "HEAD"],
        );

        let mut command = git_command(Some(&work), &global_config, &empty_hooks);
        command.args(["push", "-q"]).arg(&remote);
        for reference in [
            "refs/heads/main".to_owned(),
            "refs/heads/master".to_owned(),
            "refs/heads/retention-anchor".to_owned(),
        ]
        .iter()
        .chain(candidate_refs.iter())
        {
            command.arg(format!("HEAD:{reference}"));
        }
        checked(command);
        git(
            None,
            &global_config,
            &empty_hooks,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "symbolic-ref",
                "HEAD",
                "refs/heads/main",
            ],
        );
        git(
            None,
            &global_config,
            &empty_hooks,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "config",
                "receive.denyDeleteCurrent",
                "refuse",
            ],
        );

        Self {
            _temporary: temporary,
            remote,
            work,
            global_config,
            empty_hooks,
            old_oid,
        }
    }

    fn destination(&self) -> ValidatedDestination {
        ValidatedDestination::local_bare_fixture(&self.remote, IDENTITY).unwrap()
    }

    fn scope(&self, eligible_refs: &[String]) -> VerifiedDeletionScope {
        let oid = self.old_oid.clone();
        unsafe {
            VerifiedDeletionScope::attest(ReviewedDeletionFacts {
                repository_identity: IDENTITY.into(),
                actual_default_branch: RefAssertion {
                    reference: "refs/heads/main".into(),
                    expected: RefState::Present(oid.clone()),
                },
                main_branch: RefAssertion {
                    reference: "refs/heads/main".into(),
                    expected: RefState::Present(oid.clone()),
                },
                master_branch: RefAssertion {
                    reference: "refs/heads/master".into(),
                    expected: RefState::Present(oid.clone()),
                },
                protections_and_rulesets_reviewed: true,
                server_rejects_active_default_branch_deletion: true,
                pull_request_heads_and_bases_reviewed: true,
                keep_patterns_reviewed: true,
                retention_anchors: vec![RetentionAnchor {
                    reference: "refs/heads/retention-anchor".into(),
                    expected_oid: oid.clone(),
                    retained_candidate_oids: vec![oid.clone()],
                    retention_is_server_enforced: true,
                }],
                eligible_candidates: eligible_refs
                    .iter()
                    .map(|reference| BranchCandidate {
                        reference: reference.clone(),
                        expected_oid: oid.clone(),
                    })
                    .collect(),
            })
        }
        .unwrap()
    }

    fn commit_new_tip(&self) -> String {
        fs::write(self.work.join("new-file"), b"concurrent update\n").unwrap();
        git(
            Some(&self.work),
            &self.global_config,
            &self.empty_hooks,
            &["add", "new-file"],
        );
        git(
            Some(&self.work),
            &self.global_config,
            &self.empty_hooks,
            &["commit", "-q", "-m", "concurrent update"],
        );
        let oid = git_text(
            Some(&self.work),
            &self.global_config,
            &self.empty_hooks,
            &["rev-parse", "HEAD"],
        );
        let mut command = git_command(Some(&self.work), &self.global_config, &self.empty_hooks);
        command
            .args(["push", "-q"])
            .arg(&self.remote)
            .arg("HEAD:refs/heads/concurrent-source");
        checked(command);
        oid
    }

    fn refs(&self) -> BTreeMap<String, String> {
        let output = git(
            None,
            &self.global_config,
            &self.empty_hooks,
            &[
                "--git-dir",
                self.remote.to_str().unwrap(),
                "show-ref",
                "--heads",
            ],
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| {
                let (oid, reference) = line.split_once(' ').unwrap();
                (reference.to_owned(), oid.to_owned())
            })
            .collect()
    }
}

fn git_command(cwd: Option<&Path>, global_config: &Path, hooks: &Path) -> Command {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.arg("-C").arg(cwd);
    }
    command
        .arg("-c")
        .arg(format!("core.hooksPath={}", hooks.display()))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", global_config);
    command
}

fn git(cwd: Option<&Path>, global_config: &Path, hooks: &Path, args: &[&str]) -> Output {
    let mut command = git_command(cwd, global_config, hooks);
    command.args(args);
    checked(command)
}

fn git_text(cwd: Option<&Path>, global_config: &Path, hooks: &Path, args: &[&str]) -> String {
    String::from_utf8(git(cwd, global_config, hooks, args).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

fn checked(mut command: Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn find_git_binary() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .expect("git must be installed for the local bare-remote fixture")
}

fn install_git_wrapper(temp: &Path) -> (PathBuf, Vec<PathBuf>) {
    let bin = temp.join("wrapper-bin");
    fs::create_dir(&bin).unwrap();
    let wrapper = bin.join("git");
    fs::write(
        &wrapper,
        r#"#!/bin/sh
is_push=0
for arg in "$@"; do
  if [ "$arg" = push ]; then is_push=1; fi
done
if [ "$is_push" -eq 1 ] && [ -n "${FNR_RACE_REMOTE:-}" ]; then
  "$FNR_REAL_GIT" --git-dir="$FNR_RACE_REMOTE" update-ref "$FNR_RACE_REF" "$FNR_RACE_OID" || exit $?
fi
if [ "$is_push" -eq 1 ] && [ -n "${FNR_MOVE_DELETE_REMOTE:-}" ]; then
  "$FNR_REAL_GIT" --git-dir="$FNR_MOVE_DELETE_REMOTE" update-ref "$FNR_MOVE_DELETE_REF" "$FNR_MOVE_DELETE_OID" || exit $?
  "$FNR_REAL_GIT" --git-dir="$FNR_MOVE_DELETE_REMOTE" update-ref -d "$FNR_MOVE_DELETE_REF" "$FNR_MOVE_DELETE_OID" || exit $?
fi
if [ "$is_push" -eq 1 ] && [ -n "${FNR_RETARGET_REMOTE:-}" ]; then
  "$FNR_REAL_GIT" --git-dir="$FNR_RETARGET_REMOTE" symbolic-ref HEAD "$FNR_HEAD_TARGET" || exit $?
fi
"$FNR_REAL_GIT" "$@"
status=$?
if [ "$is_push" -eq 1 ] && [ "$status" -eq 0 ] && [ -n "${FNR_FAIL_PUSH_RESPONSE:-}" ]; then exit 1; fi
exit "$status"
"#,
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let old_path = std::env::var_os("PATH").unwrap();
    let mut entries = vec![bin];
    entries.extend(std::env::split_paths(&old_path));
    (wrapper, entries)
}

#[test]
fn atomic_deletions_are_bounded_and_uncertain_results_are_never_claimed() {
    let candidate_refs = (0..33)
        .map(|index| format!("refs/heads/remove/{index:02}"))
        .collect::<Vec<_>>();
    let fixture = Fixture::new(&candidate_refs);
    let scope = fixture.scope(&candidate_refs);
    let candidates = candidate_refs
        .iter()
        .map(|reference| BranchCandidate {
            reference: reference.clone(),
            expected_oid: fixture.old_oid.clone(),
        })
        .collect::<Vec<_>>();

    let report = delete_branches_if_unchanged(&fixture.destination(), &scope, &candidates).unwrap();
    assert_eq!(report.deleted_refs, candidate_refs);
    assert_eq!(report.transactions.len(), 2);
    assert_eq!(report.transactions[0].references.len(), 32);
    assert_eq!(report.transactions[1].references.len(), 1);
    assert!(
        report
            .transactions
            .iter()
            .all(|transaction| transaction.disposition == TransactionDisposition::Acknowledged)
    );
    let refs = fixture.refs();
    assert_eq!(refs.get("refs/heads/main"), Some(&fixture.old_oid));
    assert_eq!(refs.get("refs/heads/master"), Some(&fixture.old_oid));
    assert_eq!(
        refs.get("refs/heads/retention-anchor"),
        Some(&fixture.old_oid)
    );
    assert!(
        candidate_refs
            .iter()
            .all(|reference| !refs.contains_key(reference))
    );

    let race_refs = vec![
        "refs/heads/race/first".to_owned(),
        "refs/heads/race/second".to_owned(),
    ];
    let race = Fixture::new(&race_refs);
    let race_scope = race.scope(&race_refs);
    let new_oid = race.commit_new_tip();
    let real_git = find_git_binary();
    let (wrapper, entries) = install_git_wrapper(race._temporary.path());
    let old_path = std::env::var_os("PATH").unwrap();
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(entries).unwrap());
        std::env::set_var("FNR_REAL_GIT", &real_git);
        std::env::set_var("FNR_RACE_REMOTE", &race.remote);
        std::env::set_var("FNR_RACE_REF", &race_refs[0]);
        std::env::set_var("FNR_RACE_OID", &new_oid);
    }
    let race_candidates = race_refs
        .iter()
        .map(|reference| BranchCandidate {
            reference: reference.clone(),
            expected_oid: race.old_oid.clone(),
        })
        .collect::<Vec<_>>();
    let raced = delete_branches_if_unchanged(&race.destination(), &race_scope, &race_candidates);
    unsafe {
        std::env::set_var("PATH", old_path);
        std::env::remove_var("FNR_REAL_GIT");
        std::env::remove_var("FNR_RACE_REMOTE");
        std::env::remove_var("FNR_RACE_REF");
        std::env::remove_var("FNR_RACE_OID");
    }
    assert!(wrapper.is_file());
    let error = raced.expect_err("lease race must refuse the whole atomic batch");
    assert!(error.outcome_ambiguous);
    assert!(error.confirmed_deleted.is_empty());
    let race_after = race.refs();
    assert_eq!(race_after.get(&race_refs[0]), Some(&new_oid));
    assert_eq!(race_after.get(&race_refs[1]), Some(&race.old_oid));
    assert_eq!(
        race_after.get("refs/heads/retention-anchor"),
        Some(&race.old_oid)
    );

    let moved_deleted_ref = "refs/heads/move-then-delete".to_owned();
    let moved_deleted = Fixture::new(std::slice::from_ref(&moved_deleted_ref));
    let moved_deleted_scope = moved_deleted.scope(std::slice::from_ref(&moved_deleted_ref));
    let unanchored_oid = moved_deleted.commit_new_tip();
    let real_git = find_git_binary();
    let (_wrapper, entries) = install_git_wrapper(moved_deleted._temporary.path());
    let old_path = std::env::var_os("PATH").unwrap();
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(entries).unwrap());
        std::env::set_var("FNR_REAL_GIT", &real_git);
        std::env::set_var("FNR_MOVE_DELETE_REMOTE", &moved_deleted.remote);
        std::env::set_var("FNR_MOVE_DELETE_REF", &moved_deleted_ref);
        std::env::set_var("FNR_MOVE_DELETE_OID", &unanchored_oid);
    }
    let moved_deleted_result = delete_branches_if_unchanged(
        &moved_deleted.destination(),
        &moved_deleted_scope,
        &[BranchCandidate {
            reference: moved_deleted_ref.clone(),
            expected_oid: moved_deleted.old_oid.clone(),
        }],
    );
    unsafe {
        std::env::set_var("PATH", old_path);
        std::env::remove_var("FNR_REAL_GIT");
        std::env::remove_var("FNR_MOVE_DELETE_REMOTE");
        std::env::remove_var("FNR_MOVE_DELETE_REF");
        std::env::remove_var("FNR_MOVE_DELETE_OID");
    }
    let moved_deleted_error = moved_deleted_result
        .expect_err("move-then-delete during failed push must remain ambiguous");
    assert!(moved_deleted_error.outcome_ambiguous);
    assert!(moved_deleted_error.confirmed_deleted.is_empty());
    assert!(
        moved_deleted_error
            .message
            .contains("cannot attribute deletion")
    );
    let moved_deleted_after = moved_deleted.refs();
    assert!(!moved_deleted_after.contains_key(&moved_deleted_ref));
    assert_eq!(
        moved_deleted_after.get("refs/heads/retention-anchor"),
        Some(&moved_deleted.old_oid)
    );

    let active_ref = "refs/heads/active-default-race".to_owned();
    let active = Fixture::new(std::slice::from_ref(&active_ref));
    let active_scope = active.scope(std::slice::from_ref(&active_ref));
    let real_git = find_git_binary();
    let (_wrapper, entries) = install_git_wrapper(active._temporary.path());
    let old_path = std::env::var_os("PATH").unwrap();
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(entries).unwrap());
        std::env::set_var("FNR_REAL_GIT", &real_git);
        std::env::set_var("FNR_RETARGET_REMOTE", &active.remote);
        std::env::set_var("FNR_HEAD_TARGET", &active_ref);
    }
    let active_result = delete_branches_if_unchanged(
        &active.destination(),
        &active_scope,
        &[BranchCandidate {
            reference: active_ref.clone(),
            expected_oid: active.old_oid.clone(),
        }],
    );
    unsafe {
        std::env::set_var("PATH", old_path);
        std::env::remove_var("FNR_REAL_GIT");
        std::env::remove_var("FNR_RETARGET_REMOTE");
        std::env::remove_var("FNR_HEAD_TARGET");
    }
    let active_error = active_result.expect_err("active default branch must stay protected");
    assert!(active_error.outcome_ambiguous);
    assert!(active_error.confirmed_deleted.is_empty());
    assert_eq!(active.refs().get(&active_ref), Some(&active.old_oid));
    assert_eq!(
        git_text(
            None,
            &active.global_config,
            &active.empty_hooks,
            &[
                "--git-dir",
                active.remote.to_str().unwrap(),
                "symbolic-ref",
                "HEAD",
            ],
        ),
        active_ref
    );

    let lost_refs = vec!["refs/heads/lost-response".to_owned()];
    let lost = Fixture::new(&lost_refs);
    let lost_scope = lost.scope(&lost_refs);
    let real_git = find_git_binary();
    let (_wrapper, entries) = install_git_wrapper(lost._temporary.path());
    let old_path = std::env::var_os("PATH").unwrap();
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(entries).unwrap());
        std::env::set_var("FNR_REAL_GIT", &real_git);
        std::env::set_var("FNR_FAIL_PUSH_RESPONSE", "1");
    }
    let lost_report = delete_branches_if_unchanged(
        &lost.destination(),
        &lost_scope,
        &[BranchCandidate {
            reference: lost_refs[0].clone(),
            expected_oid: lost.old_oid.clone(),
        }],
    );
    unsafe {
        std::env::set_var("PATH", old_path);
        std::env::remove_var("FNR_REAL_GIT");
        std::env::remove_var("FNR_FAIL_PUSH_RESPONSE");
    }
    let lost_error = lost_report.expect_err("lost push response must remain ambiguous");
    assert!(lost_error.outcome_ambiguous);
    assert!(lost_error.confirmed_deleted.is_empty());
    assert!(lost_error.message.contains("cannot attribute deletion"));
    let lost_after = lost.refs();
    assert!(!lost_after.contains_key("refs/heads/lost-response"));
    assert_eq!(
        lost_after.get("refs/heads/retention-anchor"),
        Some(&lost.old_oid)
    );
}
