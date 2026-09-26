//! Conditional deletion of a duplicate remote branch.
//!
//! Recheck both refs immediately before an ordinary, non-force branch delete.

use std::path::Path;
use std::process::Command;

/// Delete `candidate_ref` only if its tip and `keeper_ref` still match the
/// caller's observed OIDs.
///
/// Both refs must be full refs (for example, `refs/heads/topic`). The candidate
/// and keeper must still point at the same expected commit, and protected
/// primary refs cannot be candidates. Git's ordinary delete push has no
/// compare-and-delete transaction; the preflight check narrows but cannot
/// eliminate the race with a concurrent remote update.
pub fn delete_if_unchanged(
    remote: &str,
    candidate_ref: &str,
    candidate_oid: &str,
    keeper_ref: &str,
    keeper_oid: &str,
) -> Result<(), String> {
    if candidate_ref == keeper_ref {
        return Err("candidate and keeper refs must differ".into());
    }
    validate_oid(candidate_oid)?;
    validate_oid(keeper_oid)?;
    validate_ref(candidate_ref)?;
    validate_ref(keeper_ref)?;
    if candidate_oid != keeper_oid {
        return Err("candidate and keeper must have the same expected commit ID".into());
    }
    let default_ref = remote_default_ref(remote)?
        .ok_or_else(|| "remote default branch is unknown; refusing delete".to_owned())?;
    if matches!(candidate_ref, "refs/heads/main" | "refs/heads/master")
        || candidate_ref.strip_prefix("refs/heads/") == Some(default_ref.as_str())
    {
        return Err(format!(
            "refusing to delete protected branch {candidate_ref}"
        ));
    }
    if remote_ref_oid(remote, candidate_ref)?.as_deref() != Some(candidate_oid) {
        return Err(format!(
            "candidate tip changed before delete: {candidate_ref}"
        ));
    }
    if remote_ref_oid(remote, keeper_ref)?.as_deref() != Some(keeper_oid) {
        return Err(format!("keeper tip changed before delete: {keeper_ref}"));
    }
    let branch = candidate_ref
        .strip_prefix("refs/heads/")
        .ok_or_else(|| format!("expected a branch ref, got: {candidate_ref}"))?;
    git(None, &delete_push_args(remote, branch))?;
    if remote_ref_oid(remote, candidate_ref)?.is_some() {
        return Err(format!("branch still exists after delete: {candidate_ref}"));
    }
    if remote_ref_oid(remote, keeper_ref)?.as_deref() != Some(keeper_oid) {
        return Err(format!("keeper tip changed during delete: {keeper_ref}"));
    }
    Ok(())
}

fn delete_push_args<'a>(remote: &'a str, branch: &'a str) -> [&'a str; 4] {
    ["push", "--delete", remote, branch]
}

fn remote_default_ref(remote: &str) -> Result<Option<String>, String> {
    let listing = git(None, &["ls-remote", "--symref", remote, "HEAD"])?;
    Ok(listing.lines().find_map(|line| {
        let (target, name) = line.split_once('\t')?;
        (name == "HEAD")
            .then(|| target.strip_prefix("ref: refs/heads/").map(str::to_owned))
            .flatten()
    }))
}

fn remote_ref_oid(remote: &str, reference: &str) -> Result<Option<String>, String> {
    let listing = git(None, &["ls-remote", "--refs", remote, reference])?;
    Ok(listing.lines().find_map(|line| {
        let (oid, name) = line.split_once('\t')?;
        (name == reference).then(|| oid.to_owned())
    }))
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
    let result = command
        .args(["check-ref-format", reference])
        .output()
        .map_err(|e| format!("run git check-ref-format: {e}"))?;
    if !result.status.success() {
        return Err(format!("invalid branch ref: {reference}"));
    }
    Ok(())
}

fn path_arg(path: &Path) -> &str {
    path.to_str().expect("temporary path must be valid UTF-8")
}

fn git(cwd: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut command = git_command(cwd);
    let output = command
        .args(args)
        .output()
        .map_err(|e| format!("could not start git {}: {e}", safe_verb(args)))?;
    if !output.status.success() {
        // Fetch/push diagnostics commonly echo remote URLs and credentials.
        if matches!(args.first(), Some(&"fetch") | Some(&"push")) {
            return Err(format!(
                "git {} failed ({})",
                safe_verb(args),
                output.status
            ));
        }
        return Err(format!(
            "git {} failed ({}): {}",
            safe_verb(args),
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

fn safe_verb<'a>(args: &[&'a str]) -> &'a str {
    args.first().copied().unwrap_or("command")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn branch_deletion_uses_no_force_option() {
        let args = delete_push_args("origin", "recovery/find-and-recovery/duplicate");
        assert_eq!(
            args,
            [
                "push",
                "--delete",
                "origin",
                "recovery/find-and-recovery/duplicate"
            ]
        );
        assert!(!args.iter().any(|argument| argument.contains("force")));
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        remote: PathBuf,
        work: PathBuf,
        candidate_oid: String,
        keeper_oid: String,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let remote = temp.path().join("remote.git");
            let work = temp.path().join("work");
            run(None, &["init", "--bare", "-q", path_arg(&remote)]);
            run(None, &["init", "-q", path_arg(&work)]);
            run(Some(&work), &["config", "user.name", "fixture"]);
            run(
                Some(&work),
                &["config", "user.email", "fixture@example.invalid"],
            );
            fs::write(work.join("file.txt"), "same content\n").unwrap();
            run(Some(&work), &["add", "file.txt"]);
            run(Some(&work), &["commit", "-m", "fixture"]);
            run(Some(&work), &["branch", "keeper"]);
            run(Some(&work), &["branch", "candidate"]);
            run(Some(&work), &["remote", "add", "origin", path_arg(&remote)]);
            run(
                Some(&work),
                &["push", "-q", "origin", "HEAD:refs/heads/keeper"],
            );
            run(
                Some(&work),
                &["push", "-q", "origin", "HEAD:refs/heads/candidate"],
            );
            for branch in ["main", "master", "default"] {
                run(
                    Some(&work),
                    &["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")],
                );
            }
            run(
                None,
                &[
                    "--git-dir",
                    path_arg(&remote),
                    "symbolic-ref",
                    "HEAD",
                    "refs/heads/default",
                ],
            );
            let candidate_oid = output(Some(&work), &["rev-parse", "HEAD"]);
            let keeper_oid = candidate_oid.clone();
            Self {
                _temp: temp,
                remote,
                work,
                candidate_oid,
                keeper_oid,
            }
        }

        fn remote_oid(&self, reference: &str) -> Option<String> {
            let result = Command::new("git")
                .args(["--git-dir"])
                .arg(&self.remote)
                .args(["rev-parse", "--verify", reference])
                .output()
                .unwrap();
            result
                .status
                .success()
                .then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
        }

        fn advance_branch(&self, branch: &str, content: &str) -> String {
            run(Some(&self.work), &["checkout", "-q", "--detach", "HEAD"]);
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
    fn deletes_exact_candidate_with_ordinary_push_and_preserves_keeper() {
        let fixture = Fixture::new();
        delete_if_unchanged(
            fixture.remote.to_str().unwrap(),
            "refs/heads/candidate",
            &fixture.candidate_oid,
            "refs/heads/keeper",
            &fixture.keeper_oid,
        )
        .unwrap();
        assert_eq!(fixture.remote_oid("refs/heads/candidate"), None);
        assert_eq!(
            fixture.remote_oid("refs/heads/keeper").as_deref(),
            Some(fixture.keeper_oid.as_str())
        );
    }

    #[test]
    fn moved_candidate_is_retained() {
        let fixture = Fixture::new();
        let moved_candidate = fixture.advance_branch("candidate", "candidate moved\n");
        let result = delete_if_unchanged(
            fixture.remote.to_str().unwrap(),
            "refs/heads/candidate",
            &fixture.candidate_oid,
            "refs/heads/keeper",
            &fixture.keeper_oid,
        );
        assert!(result.is_err());
        assert_eq!(
            fixture.remote_oid("refs/heads/candidate").as_deref(),
            Some(moved_candidate.as_str())
        );
        assert_eq!(
            fixture.remote_oid("refs/heads/keeper").as_deref(),
            Some(fixture.keeper_oid.as_str())
        );
    }

    #[test]
    fn moved_keeper_keeps_candidate() {
        let fixture = Fixture::new();
        let moved_keeper = fixture.advance_branch("keeper", "keeper moved\n");
        let result = delete_if_unchanged(
            fixture.remote.to_str().unwrap(),
            "refs/heads/candidate",
            &fixture.candidate_oid,
            "refs/heads/keeper",
            &fixture.keeper_oid,
        );
        assert!(result.is_err());
        assert_eq!(
            fixture.remote_oid("refs/heads/candidate").as_deref(),
            Some(fixture.candidate_oid.as_str())
        );
        assert_eq!(
            fixture.remote_oid("refs/heads/keeper").as_deref(),
            Some(moved_keeper.as_str())
        );
    }

    #[test]
    fn main_master_and_advertised_default_are_protected() {
        let fixture = Fixture::new();
        for branch in ["main", "master", "default"] {
            let error = delete_if_unchanged(
                fixture.remote.to_str().unwrap(),
                &format!("refs/heads/{branch}"),
                &fixture.candidate_oid,
                "refs/heads/keeper",
                &fixture.keeper_oid,
            )
            .unwrap_err();
            assert!(error.contains("protected branch"), "{branch}: {error}");
            assert_eq!(
                fixture
                    .remote_oid(&format!("refs/heads/{branch}"))
                    .as_deref(),
                Some(fixture.candidate_oid.as_str())
            );
        }
    }
}
