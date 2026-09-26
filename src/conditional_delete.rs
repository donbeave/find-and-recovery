//! Atomic deletion of archived duplicate remote branches.
//!
//! The caller archives every candidate before this module runs. Those archive
//! refs preserve candidate commits independently, so no keeper relation is
//! required here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    /// Full branch ref, such as `refs/heads/topic`.
    pub remote_ref: String,
    /// Expected tip observed when the candidate was archived.
    pub expected_oid: String,
    pub keeper_ref: String,
    pub keeper_oid: String,
}

/// Delete exact-OID aliases only, rechecking both refs immediately before each
/// plain delete push. Git has no non-force compare-and-delete transaction, so
/// a remote update racing after the final check cannot be excluded.
pub fn delete_candidates_if_unchanged(
    remote: &str,
    candidates: &[Candidate],
) -> Result<(), String> {
    if candidates.is_empty() {
        return Ok(());
    }
    let mut seen = BTreeSet::new();
    for candidate in candidates {
        validate_oid(&candidate.expected_oid)?;
        validate_oid(&candidate.keeper_oid)?;
        validate_ref(&candidate.remote_ref)?;
        validate_ref(&candidate.keeper_ref)?;
        if !seen.insert(candidate.remote_ref.clone()) {
            return Err(format!("duplicate candidate ref: {}", candidate.remote_ref));
        }
    }
    let default = remote_default_branch(remote)?
        .ok_or_else(|| "remote default branch is unknown; refusing delete".to_owned())?;
    for candidate in candidates {
        let name = candidate.remote_ref.strip_prefix("refs/heads/").unwrap();
        if matches!(name, "main" | "master") || name == default {
            return Err(format!("refusing to delete protected branch {name}"));
        }
        if candidate.remote_ref == candidate.keeper_ref
            || candidate.expected_oid != candidate.keeper_oid
        {
            return Err("candidate must be a distinct exact-OID alias of its keeper".into());
        }
    }

    // Validate the full observed plan before the first delete.
    for candidate in candidates {
        let refs = [candidate.remote_ref.as_str(), candidate.keeper_ref.as_str()];
        let observed = remote_ref_oids(remote, &refs)?;
        if observed.get(&candidate.remote_ref).map(String::as_str)
            != Some(candidate.expected_oid.as_str())
            || observed.get(&candidate.keeper_ref).map(String::as_str)
                != Some(candidate.keeper_oid.as_str())
        {
            return Err(format!(
                "candidate or keeper changed before delete: {}",
                candidate.remote_ref
            ));
        }
    }

    for candidate in candidates {
        let refs = [candidate.remote_ref.as_str(), candidate.keeper_ref.as_str()];
        let observed = remote_ref_oids(remote, &refs)?;
        if observed.get(&candidate.remote_ref).map(String::as_str)
            != Some(candidate.expected_oid.as_str())
            || observed.get(&candidate.keeper_ref).map(String::as_str)
                != Some(candidate.keeper_oid.as_str())
        {
            return Err(format!(
                "candidate or keeper changed before delete: {}",
                candidate.remote_ref
            ));
        }
        let branch = candidate.remote_ref.strip_prefix("refs/heads/").unwrap();
        let args = delete_push_args(remote, branch);
        git(None, &args)?;
        let remaining = remote_ref_oids(remote, &refs)?;
        if remaining.contains_key(&candidate.remote_ref) {
            return Err(format!(
                "candidate branch remains after delete: {}",
                candidate.remote_ref
            ));
        }
        if remaining.get(&candidate.keeper_ref).map(String::as_str)
            != Some(candidate.keeper_oid.as_str())
        {
            return Err(format!(
                "keeper changed after delete: {}",
                candidate.keeper_ref
            ));
        }
    }
    Ok(())
}

/// Refuse the legacy API so old callers cannot bypass the exact-tip checks.
#[deprecated(note = "archive candidates and call delete_candidates_if_unchanged")]
pub fn delete_if_unchanged(
    _remote: &str,
    _candidate_ref: &str,
    _candidate_oid: &str,
    _keeper_ref: &str,
    _keeper_oid: &str,
) -> Result<(), String> {
    Err("keeper-based deletion API removed; use delete_candidates_if_unchanged".into())
}

fn remote_default_branch(remote: &str) -> Result<Option<String>, String> {
    let listing = git(None, &["ls-remote", "--symref", "--", remote, "HEAD"])?;
    Ok(listing.lines().find_map(|line| {
        let (target, label) = line.split_once('\t')?;
        (label == "HEAD")
            .then(|| target.strip_prefix("ref: refs/heads/").map(str::to_owned))
            .flatten()
    }))
}

fn delete_push_args<'a>(remote: &'a str, branch: &'a str) -> [&'a str; 4] {
    ["push", "--delete", remote, branch]
}

fn remote_ref_oids(remote: &str, refs: &[&str]) -> Result<BTreeMap<String, String>, String> {
    let mut args = vec!["ls-remote", "--refs", "--", remote];
    args.extend_from_slice(refs);
    let listing = git(None, &args)?;
    Ok(listing
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            refs.contains(&name)
                .then(|| (name.to_owned(), oid.to_owned()))
        })
        .collect())
}

fn validate_oid(oid: &str) -> Result<(), String> {
    if (oid.len() != 40 && oid.len() != 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid expected object ID: {oid}"));
    }
    Ok(())
}

fn validate_ref(reference: &str) -> Result<(), String> {
    if !reference.starts_with("refs/heads/") {
        return Err(format!("expected a branch ref, got: {reference}"));
    }
    let mut command = git_command(None);
    let output = command
        .args(["check-ref-format", reference])
        .output()
        .map_err(|e| format!("run git check-ref-format: {e}"))?;
    if !output.status.success() {
        return Err(format!("invalid branch ref: {reference}"));
    }
    Ok(())
}

fn git(cwd: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut command = git_command(cwd);
    let output = command.args(args).output().map_err(|e| {
        format!(
            "could not start git {}: {e}",
            args.first().copied().unwrap_or("command")
        )
    })?;
    if !output.status.success() {
        if matches!(args.first(), Some(&"fetch") | Some(&"push")) {
            return Err(format!("git {} failed ({})", args[0], output.status));
        }
        return Err(format!(
            "git {} failed ({}): {}",
            args.first().copied().unwrap_or("command"),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_command(cwd: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.arg("-C").arg(cwd);
    }
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if matches!(
            name.as_ref(),
            "GIT_DIR"
                | "GIT_WORK_TREE"
                | "GIT_COMMON_DIR"
                | "GIT_OBJECT_DIRECTORY"
                | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                | "GIT_INDEX_FILE"
        ) || name.starts_with("GIT_CONFIG_")
        {
            command.env_remove(key);
        }
    }
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        _temp: tempfile::TempDir,
        remote: std::path::PathBuf,
        work: std::path::PathBuf,
        base_oid: String,
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
            fs::write(work.join("file.txt"), "same content\n").unwrap();
            run(Some(&work), &["add", "file.txt"]);
            run(Some(&work), &["commit", "-m", "fixture"]);
            run(
                Some(&work),
                &["remote", "add", "origin", remote.to_str().unwrap()],
            );
            for branch in ["candidate-a", "candidate-b", "main", "master", "default"] {
                run(
                    Some(&work),
                    &["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")],
                );
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
            let base_oid = output(Some(&work), &["rev-parse", "HEAD"]);
            Self {
                _temp: temp,
                remote,
                work,
                base_oid,
            }
        }

        fn candidate(&self, branch: &str, oid: &str) -> Candidate {
            Candidate {
                remote_ref: format!("refs/heads/{branch}"),
                expected_oid: oid.to_owned(),
                keeper_ref: "refs/heads/main".to_owned(),
                keeper_oid: self.base_oid.clone(),
            }
        }

        fn remote_oid(&self, reference: &str) -> Option<String> {
            let output = Command::new("git")
                .args(["--git-dir"])
                .arg(&self.remote)
                .args(["rev-parse", "--verify", reference])
                .output()
                .unwrap();
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }

        fn advance_branch(&self, branch: &str, content: &str) -> String {
            run(
                Some(&self.work),
                &["checkout", "-q", "--detach", &self.base_oid],
            );
            fs::write(self.work.join("file.txt"), content).unwrap();
            run(Some(&self.work), &["commit", "-am", "advance"]);
            let oid = output(Some(&self.work), &["rev-parse", "HEAD"]);
            run(
                Some(&self.work),
                &["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")],
            );
            oid
        }
    }

    fn output(cwd: Option<&Path>, args: &[&str]) -> String {
        git(cwd, args).unwrap().trim().to_owned()
    }

    fn run(cwd: Option<&Path>, args: &[&str]) {
        git(cwd, args).unwrap();
    }

    #[test]
    fn delete_push_has_no_force_or_custom_refspec() {
        let args = delete_push_args("origin", "recovery/find-and-recovery/x");
        assert_eq!(
            args,
            ["push", "--delete", "origin", "recovery/find-and-recovery/x"]
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("force") || arg.contains(':'))
        );
    }

    #[test]
    fn multiple_candidates_delete_sequentially_after_rechecks() {
        let fixture = Fixture::new();
        let candidates = vec![
            fixture.candidate("candidate-a", &fixture.base_oid),
            fixture.candidate("candidate-b", &fixture.base_oid),
        ];
        delete_candidates_if_unchanged(fixture.remote.to_str().unwrap(), &candidates).unwrap();
        assert_eq!(fixture.remote_oid("refs/heads/candidate-a"), None);
        assert_eq!(fixture.remote_oid("refs/heads/candidate-b"), None);
        assert_eq!(
            fixture.remote_oid("refs/heads/main").as_deref(),
            Some(fixture.base_oid.as_str())
        );
    }

    #[test]
    fn stale_candidate_blocks_every_candidate_before_push() {
        let fixture = Fixture::new();
        let moved = fixture.advance_branch("candidate-b", "moved\n");
        let candidates = vec![
            fixture.candidate("candidate-a", &fixture.base_oid),
            fixture.candidate("candidate-b", &fixture.base_oid),
        ];
        let error = delete_candidates_if_unchanged(fixture.remote.to_str().unwrap(), &candidates)
            .unwrap_err();
        assert!(error.contains("candidate-b"));
        assert_eq!(
            fixture.remote_oid("refs/heads/candidate-a").as_deref(),
            Some(fixture.base_oid.as_str())
        );
        assert_eq!(
            fixture.remote_oid("refs/heads/candidate-b").as_deref(),
            Some(moved.as_str())
        );
    }

    #[test]
    fn main_master_and_advertised_default_are_protected() {
        let fixture = Fixture::new();
        for branch in ["main", "master", "default"] {
            let error = delete_candidates_if_unchanged(
                fixture.remote.to_str().unwrap(),
                &[fixture.candidate(branch, &fixture.base_oid)],
            )
            .unwrap_err();
            assert!(error.contains("protected branch"), "{branch}: {error}");
        }
        assert_eq!(
            fixture.remote_oid("refs/heads/main").as_deref(),
            Some(fixture.base_oid.as_str())
        );
        assert_eq!(
            fixture.remote_oid("refs/heads/master").as_deref(),
            Some(fixture.base_oid.as_str())
        );
        assert_eq!(
            fixture.remote_oid("refs/heads/default").as_deref(),
            Some(fixture.base_oid.as_str())
        );
    }

    #[test]
    fn non_head_ref_is_rejected_without_remote_contact() {
        let error = delete_candidates_if_unchanged(
            "file:///must-not-be-contacted",
            &[Candidate {
                remote_ref: "refs/tags/release".into(),
                expected_oid: "a".repeat(40),
                keeper_ref: "refs/heads/main".into(),
                keeper_oid: "a".repeat(40),
            }],
        )
        .unwrap_err();
        assert!(error.contains("expected a branch ref"));
    }
}
