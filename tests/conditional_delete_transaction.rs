#[path = "../src/conditional_delete.rs"]
mod conditional_delete;

use conditional_delete::{Candidate, delete_candidates_if_unchanged};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    remote: PathBuf,
    work: PathBuf,
    base: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        let work = temp.path().join("work");
        run(None, &["init", "--bare", "-q", remote.to_str().unwrap()]);
        run(None, &["init", "-q", work.to_str().unwrap()]);
        run(Some(&work), &["config", "user.name", "fixture"]);
        run(
            Some(&work),
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(work.join("file"), "first\n").unwrap();
        run(Some(&work), &["add", "file"]);
        run(Some(&work), &["commit", "-m", "base"]);
        let base = output(Some(&work), &["rev-parse", "HEAD"]);
        for branch in ["candidate-a", "candidate-b", "main", "master", "default"] {
            push_head(&work, &remote, branch);
        }
        run(
            None,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "symbolic-ref",
                "HEAD",
                "refs/heads/default",
            ],
        );
        Self {
            _temp: temp,
            remote,
            work,
            base,
        }
    }

    fn candidate(&self, branch: &str, keeper: &str) -> Candidate {
        Candidate {
            remote_ref: format!("refs/heads/{branch}"),
            expected_oid: self.base.clone(),
            keeper_ref: format!("refs/heads/{keeper}"),
            keeper_oid: self.base.clone(),
        }
    }
    fn second(&self) -> String {
        std::fs::write(self.work.join("file"), "second\n").unwrap();
        run(Some(&self.work), &["add", "file"]);
        run(Some(&self.work), &["commit", "-m", "second"]);
        let oid = output(Some(&self.work), &["rev-parse", "HEAD"]);
        push_head(&self.work, &self.remote, "moving");
        oid
    }
    fn remote_oid(&self, reference: &str) -> Option<String> {
        let o = git(
            None,
            &[
                "--git-dir",
                self.remote.to_str().unwrap(),
                "rev-parse",
                "--verify",
                reference,
            ],
        );
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    }
    fn set_remote_oid(&self, reference: &str, oid: &str) {
        run(
            None,
            &[
                "--git-dir",
                self.remote.to_str().unwrap(),
                "update-ref",
                reference,
                oid,
            ],
        );
    }

    fn set_remote_config(&self, key: &str, value: &str) {
        run(
            None,
            &[
                "--git-dir",
                self.remote.to_str().unwrap(),
                "config",
                key,
                value,
            ],
        );
    }
    fn refs(&self, pattern: &str) -> Vec<(String, String)> {
        let o = git(
            None,
            &[
                "--git-dir",
                self.remote.to_str().unwrap(),
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                pattern,
            ],
        );
        assert!(o.status.success());
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| {
                let (oid, r) = l.split_once(' ')?;
                Some((r.to_owned(), oid.to_owned()))
            })
            .collect()
    }
    fn path(&self) -> &str {
        self.remote.to_str().unwrap()
    }
    fn hook_log(&self) -> String {
        std::fs::read_to_string(self.remote.join("hook.log")).unwrap_or_default()
    }
}

fn push_head(work: &Path, remote: &Path, branch: &str) {
    let r = format!("HEAD:refs/heads/{branch}");
    run(Some(work), &["push", "-q", remote.to_str().unwrap(), &r]);
}
fn git(cwd: Option<&Path>, args: &[&str]) -> Output {
    let mut c = Command::new("git");
    if let Some(p) = cwd {
        c.arg("-C").arg(p);
    }
    c.args(args).output().unwrap()
}
fn output(cwd: Option<&Path>, args: &[&str]) -> String {
    let o = git(cwd, args);
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_owned()
}
fn run(cwd: Option<&Path>, args: &[&str]) {
    let o = git(cwd, args);
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn hook(f: &Fixture, body: &str) {
    let p = f.remote.join("hooks/pre-receive");
    std::fs::write(&p, body).unwrap();
    assert!(
        Command::new("chmod")
            .arg("+x")
            .arg(p)
            .status()
            .unwrap()
            .success()
    );
}
fn log_line(f: &Fixture) -> String {
    format!(
        "printf '%s %s %s\\n' \"$old\" \"$new\" \"$ref\" >> {}\n",
        q(f.remote.join("hook.log").to_str().unwrap())
    )
}
fn zero_oid(f: &Fixture) -> String {
    "0".repeat(f.base.len())
}

#[test]
fn candidate_delete_is_atomic_with_keeper_anchor() {
    let f = Fixture::new();
    delete_candidates_if_unchanged(f.path(), &[f.candidate("candidate-a", "main")]).unwrap();
    assert_eq!(f.remote_oid("refs/heads/candidate-a"), None);
    let a = f.refs(&format!("refs/tags/find-and-recovery-retention/{}", f.base));
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].1, f.base);
}

#[test]
fn remote_without_atomic_push_capability_retains_every_candidate() {
    let f = Fixture::new();
    f.set_remote_config("receive.advertiseAtomic", "false");
    let result = delete_candidates_if_unchanged(f.path(), &[f.candidate("candidate-a", "main")]);
    assert!(result.is_err());
    assert_eq!(f.remote_oid("refs/heads/candidate-a"), Some(f.base.clone()));
    assert!(f.refs("refs/tags/find-and-recovery-retention").is_empty());
}

#[test]
fn moved_candidate_is_not_deleted() {
    let f = Fixture::new();
    let next = f.second();
    f.set_remote_oid("refs/heads/candidate-a", &next);
    let e = delete_candidates_if_unchanged(f.path(), &[f.candidate("candidate-a", "main")])
        .unwrap_err();
    assert!(e.contains("changed since the plan"));
    assert_eq!(f.remote_oid("refs/heads/candidate-a"), Some(next));
    assert!(f.refs("refs/tags/find-and-recovery-retention").is_empty());
}

#[test]
fn concurrent_candidate_move_rejects_atomic_transaction_without_fallback() {
    let f = Fixture::new();
    let next = f.second();
    let script = format!(
        "#!/bin/sh\nwhile read old new ref; do\n{}if [ \"$ref\" = refs/heads/candidate-a ] && [ \"$new\" = {} ]; then env -u GIT_QUARANTINE_PATH -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES git --git-dir={} update-ref refs/heads/candidate-a {} >> {} 2>&1; fi\ndone\n",
        log_line(&f),
        zero_oid(&f),
        q(f.remote.to_str().unwrap()),
        next,
        q(f.remote.join("hook.log").to_str().unwrap())
    );
    hook(&f, &script);
    let result = delete_candidates_if_unchanged(f.path(), &[f.candidate("candidate-a", "main")]);
    assert!(result.is_err(), "hook log: {}", f.hook_log());
    assert_eq!(f.remote_oid("refs/heads/candidate-a"), Some(next));
    assert!(f.refs("refs/tags/find-and-recovery-retention").is_empty());
}

#[test]
fn changed_keeper_before_mutation_blocks() {
    let f = Fixture::new();
    let next = f.second();
    f.set_remote_oid("refs/heads/main", &next);
    let e = delete_candidates_if_unchanged(f.path(), &[f.candidate("candidate-a", "main")])
        .unwrap_err();
    assert!(e.contains("changed since the plan"));
    assert_eq!(f.remote_oid("refs/heads/candidate-a"), Some(f.base.clone()));
    assert!(f.refs("refs/tags/find-and-recovery-retention").is_empty());
}

#[test]
fn keeper_move_inside_receive_pack_stays_recoverable() {
    let f = Fixture::new();
    let next = f.second();
    let script = format!(
        "#!/bin/sh\nwhile read old new ref; do\n{}if [ \"$ref\" = refs/heads/candidate-a ] && [ \"$new\" = {} ]; then env -u GIT_QUARANTINE_PATH -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES git --git-dir={} update-ref refs/heads/main {} >> {} 2>&1; fi\ndone\n",
        log_line(&f),
        zero_oid(&f),
        q(f.remote.to_str().unwrap()),
        next,
        q(f.remote.join("hook.log").to_str().unwrap())
    );
    hook(&f, &script);
    delete_candidates_if_unchanged(f.path(), &[f.candidate("candidate-a", "main")])
        .unwrap_or_else(|e| panic!("{e}; hook: {}", f.hook_log()));
    assert_eq!(f.remote_oid("refs/heads/candidate-a"), None);
    assert_eq!(f.remote_oid("refs/heads/main"), Some(next));
    let a = f.refs(&format!("refs/tags/find-and-recovery-retention/{}", f.base));
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].1, f.base);
}

#[test]
fn concurrent_anchor_collision_blocks_candidate_delete() {
    let f = Fixture::new();
    let next = f.second();
    let script = format!(
        "#!/bin/sh\nwhile read old new ref; do\n{}case \"$ref\" in refs/tags/find-and-recovery-retention/*) env -u GIT_QUARANTINE_PATH -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES git --git-dir={} update-ref \"$ref\" {} >> {} 2>&1 ;; esac\ndone\n",
        log_line(&f),
        q(f.remote.to_str().unwrap()),
        next,
        q(f.remote.join("hook.log").to_str().unwrap())
    );
    hook(&f, &script);
    let result = delete_candidates_if_unchanged(f.path(), &[f.candidate("candidate-a", "main")]);
    assert!(result.is_err(), "hook log: {}", f.hook_log());
    assert_eq!(f.remote_oid("refs/heads/candidate-a"), Some(f.base.clone()));
    let a = f.refs("refs/tags/find-and-recovery-retention");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].1, next);
}

#[test]
fn main_master_and_remote_default_are_protected() {
    let f = Fixture::new();
    for (branch, keeper) in [
        ("main", "default"),
        ("master", "default"),
        ("default", "main"),
    ] {
        assert!(
            delete_candidates_if_unchanged(f.path(), &[f.candidate(branch, keeper)])
                .unwrap_err()
                .contains("protected branch")
        );
    }
    for b in ["candidate-a", "candidate-b", "main", "master", "default"] {
        assert_eq!(
            f.remote_oid(&format!("refs/heads/{b}")),
            Some(f.base.clone())
        );
    }
    assert!(f.refs("refs/tags/find-and-recovery-retention").is_empty());
}

#[test]
fn unrelated_history_is_blocked_and_true_ancestor_is_anchored() {
    let f = Fixture::new();
    run(Some(&f.work), &["checkout", "--orphan", "other"]);
    run(Some(&f.work), &["rm", "-rf", "."]);
    std::fs::write(f.work.join("other"), "root\n").unwrap();
    run(Some(&f.work), &["add", "other"]);
    run(Some(&f.work), &["commit", "-m", "other"]);
    let other = output(Some(&f.work), &["rev-parse", "HEAD"]);
    push_head(&f.work, &f.remote, "other");
    let mut candidate = f.candidate("candidate-a", "other");
    candidate.keeper_oid = other;
    assert!(
        delete_candidates_if_unchanged(f.path(), &[candidate])
            .unwrap_err()
            .contains("not reachable")
    );
    assert_eq!(f.remote_oid("refs/heads/candidate-a"), Some(f.base.clone()));

    let g = Fixture::new();
    let child = g.second();
    let mut candidate = g.candidate("candidate-a", "moving");
    candidate.keeper_oid = child.clone();
    delete_candidates_if_unchanged(g.path(), &[candidate]).unwrap();
    assert_eq!(g.remote_oid("refs/heads/candidate-a"), None);
    let a = g.refs(&format!("refs/tags/find-and-recovery-retention/{child}"));
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].1, child);
}
