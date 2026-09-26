//! Remote branch deletion is intentionally disabled.

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

/// Refuse all remote branch deletions. Recovery tooling must never delete
/// remote branches, including exact duplicate aliases.
pub fn delete_candidates_if_unchanged(
    _remote: &str,
    _candidates: &[Candidate],
) -> Result<(), String> {
    Err("remote branch deletion is disabled by policy".into())
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
    }

    fn output(cwd: Option<&Path>, args: &[&str]) -> String {
        git(cwd, args).unwrap().trim().to_owned()
    }

    fn run(cwd: Option<&Path>, args: &[&str]) {
        git(cwd, args).unwrap();
    }

    #[test]
    fn every_remote_branch_delete_request_is_rejected_without_remote_mutation() {
        let fixture = Fixture::new();
        let candidates = vec![
            fixture.candidate("candidate-a", &fixture.base_oid),
            fixture.candidate("candidate-b", &fixture.base_oid),
        ];
        let error = delete_candidates_if_unchanged(fixture.remote.to_str().unwrap(), &candidates)
            .unwrap_err();
        assert!(error.contains("disabled by policy"));
        for branch in ["candidate-a", "candidate-b", "main", "master", "default"] {
            assert_eq!(
                fixture.remote_oid(&format!("refs/heads/{branch}")),
                Some(fixture.base_oid.clone())
            );
        }
    }
}
